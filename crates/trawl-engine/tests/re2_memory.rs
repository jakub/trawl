// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What the bundled `DuckDB`'s RE2 holds in memory for the batch lane's
//! `regexp_matches` and `regexp_extract`, for 256 of the worst patterns
//! ADR-0063's limits admit in one query, and whether the query database's
//! `memory_limit` (trawld's `query_memory_limit`) sees that memory.
//!
//! ADR-0063 ships RE2's memory as a measured envelope (human ruling,
//! 2026-11-02): this harness measures it and prints the transcript that
//! `scripts/evidence/issue-335/re2.sh` commits. Nothing here enforces a
//! bound. The measuring parts are `#[ignore]`d, because a resident-memory
//! figure on a shared machine is evidence for a reader, not a CI gate. The
//! tests that are not ignored check the harness itself.
//!
//! # Method
//!
//! Every query runs through the engine's own path: a query database opened
//! by [`Executor::open_query_database`] under [`QueryBudgets`], as trawld
//! opens it, and [`Executor::run_query`] over parquet files, so the SQL and
//! its bound regex parameters are what the emitter writes. `DuckDB` scans
//! one parquet file per task, and the corpus is [`FILES`] files, so a query
//! runs on as many threads as the database allows.
//!
//! Each measurement runs in a child process of its own (the `child` test),
//! because the C++ allocator keeps freed memory for reuse: a second
//! measurement in the same process would reuse the first one's freed heap
//! and read low. The child opens the database, warms every connection with
//! a query that reads the corpus through one trivial regex, then samples
//! its own `VmRSS` (`/proc/self/status`) and `Private_Dirty`
//! (`/proc/self/smaps_rollup`) every 10 ms on a sampler thread while the
//! measured queries run, and `duckdb_memory()` every 50 ms on a connection
//! of its own. It reports the baseline before the queries, the peak during
//! them, and what is still resident after every statement has dropped,
//! with the connections still open. A watchdog interrupts every query when
//! resident memory grows by more than the cap ([`RSS_CAP`]), so a point
//! that would exhaust the host stops instead, and the transcript says so.
//!
//! # The worst admitted patterns
//!
//! RE2 bounds each compiled regex by its `max_mem`, 8 MiB by default, which
//! `DuckDB` does not change: two thirds for the forward program and its
//! lazy DFAs, one third for the reverse program and its DFA. A pattern
//! whose DFA meets a new state on almost every byte fills its cache to that
//! budget. `regexp_extract` also runs the reverse program to find where a
//! match starts, so it might hold more than `regexp_matches`.
//!
//! The `probe` part measures a few families at the edge of admission, each
//! as 256 distinct patterns in one query (the query's pattern cap), with
//! the largest count `trawl_core::parser::parse` admits for the whole
//! query. So each passes every limit a query passes: the 64-position repeat
//! budget, the 1024-byte body cap, the 2^24-unit translation budget, the
//! 1 MiB Rust program, the 512 KiB batch view, the 256-pattern cap, and
//! the 65 536-byte query cap, which leaves 256 copies of a pattern under
//! 256 bytes each on average. It keeps the family whose peak resident
//! growth is largest, at one thread, and writes the choice into the work
//! directory for the later parts.
//!
//! # The sweep
//!
//! The later parts run the chosen family over a larger corpus, each point
//! [`REPS`] times:
//!
//! - `patterns`: 1, 64 and 256 patterns in one query, on one thread;
//! - `threads`: 256 patterns on 1, 2 and 4 database threads;
//! - `queries`: 256 patterns as one query and as `max_concurrent_queries`
//!   queries at once on clones of one database, on 1 and 4 threads;
//! - `limit`: the 256-pattern, 4-thread point with `memory_limit` at
//!   [`LOW_MEMORY_LIMIT`], far below the growth the sweep sees. A query
//!   that still passes holds RE2 memory the limit does not count.
//!
//! Each part writes its results into the work directory, and `summary`
//! reads them back and states the envelope.
//!
//! ```sh
//! cargo test -p trawl-engine --test re2_memory --release -- --ignored --nocapture --test-threads=1 --exact probe
//! ```

use std::fmt::Write as _;
use std::num::{NonZeroU32, NonZeroU64};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use duckdb::Connection;
use serde::{Deserialize, Serialize};
use trawl_core::parser::parse;
use trawl_core::schema::FieldTypes;
use trawl_engine::executor::{Executor, QueryBudgets};
use trawl_engine::scratch::ScratchLock;

const MIB: u64 = 1 << 20;

/// `trawl_config::MAX_DEFAULT_CONCURRENT_QUERIES`: the
/// `max_concurrent_queries` default is one executor permit per CPU, at most
/// this many. trawl-engine does not depend on trawl-config, so the value is
/// repeated here, and the transcript prints the host's CPU count beside it.
const MAX_DEFAULT_CONCURRENT_QUERIES: usize = 8;

/// `trawl_config::DEFAULT_QUERY_MEMORY_LIMIT`, 2 GiB, repeated for the same
/// reason.
const DEFAULT_QUERY_MEMORY_LIMIT: u64 = 2 << 30;

/// `trawl_config::DEFAULT_QUERY_THREADS`, repeated for the same reason.
const DEFAULT_QUERY_THREADS: u32 = 4;

/// The `max_concurrent_queries` default on this host.
fn max_concurrent_queries() -> usize {
    thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .min(MAX_DEFAULT_CONCURRENT_QUERIES)
}

/// The query's pattern cap, `MAX_QUERY_PATTERNS`.
const PATTERNS: usize = 256;

/// Repeats of every measured point.
const REPS: usize = 3;

/// The corpus is this many parquet files, so a scan has a task per file
/// and a query runs on every thread the database has.
const FILES: usize = 16;

/// Rows per file in the probe's corpus, about 36 000 characters in all:
/// enough that one thread fills each pattern's cache (a cache fills after
/// about 19 000 characters).
const PROBE_ROWS_PER_FILE: usize = 8;

/// Rows per file in the sweep's corpus, about 90 000 characters in all, so
/// that each of 4 threads still reads enough to fill its copy's cache.
const SWEEP_ROWS_PER_FILE: usize = 20;

/// The `memory_limit` the `limit` part reruns under: far below what RE2
/// grows by at 256 patterns.
const LOW_MEMORY_LIMIT: u64 = 64 * MIB;

/// The watchdog's cap on resident growth over a child's baseline, unless
/// `RE2_RSS_CAP_MIB` sets another.
const RSS_CAP: u64 = 12 << 30;

/// The extract stages one query may hold: the emitter's pipeline-stage
/// limit. Two named groups a stage reach 256 patterns.
const MAX_STAGES: usize = 128;

// ---------------------------------------------------------------------
// Families
// ---------------------------------------------------------------------

/// A subject alphabet: `a` starts a needle, `b` is filler and `c` ends an
/// `extract` needle.
#[derive(Clone, Copy)]
struct Alphabet {
    a: char,
    b: char,
    c: char,
}

/// Deseret letters: 4-byte, with case folds. Part 1's transcript found
/// them the costliest subject for both engines.
const DESERET: Alphabet = Alphabet {
    a: '\u{10428}',
    b: '\u{10429}',
    c: '\u{1042A}',
};

/// CJK ideographs, ASCII letters and the Specials block: the rows that
/// vary the corpus beside the needle's own alphabet.
const OTHERS: [Alphabet; 3] = [
    Alphabet {
        a: '\u{6F22}',
        b: '\u{5B57}',
        c: '\u{6587}',
    },
    Alphabet {
        a: 'a',
        b: 'b',
        c: 'c',
    },
    Alphabet {
        a: '\u{FFFD}',
        b: '\u{FFFC}',
        c: '\u{FFFB}',
    },
];

/// Which batch function a family runs in.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
enum Kind {
    /// A search-stage `/re/`, `regexp_matches` in SQL. The needle never
    /// matches, so every call scans its whole row.
    Matches,
    /// An `extract "(?P<g>re)(?P<h>)"` stage, two `regexp_extract` calls
    /// in SQL, each with its own RE2. Every row ends in a match, so RE2
    /// also runs its reverse program to find where the match starts.
    Extract,
}

/// One shape of needle over [`DESERET`].
struct Family {
    name: &'static str,
    kind: Kind,
    /// The needle at count `k`.
    body: fn(usize) -> String,
}

fn class3() -> String {
    format!("[{}{}{}]", DESERET.a, DESERET.b, DESERET.c)
}

fn not3() -> String {
    format!("[^{}{}{}]", DESERET.a, DESERET.b, DESERET.c)
}

/// The last level of FIX7's fan class over Deseret (part 1's `fan`, in
/// `tests/regex_cost.rs`): every other continuation byte under Deseret's
/// own 3-byte prefix, beside `a`, `b` and `c`. Its byte ranges are many,
/// so every DFA state carries a wide transition table. Part 1's whole fan
/// is about 800 bytes, and 256 copies of it are over the 65 536-byte query
/// cap; this part of it fits.
fn fan() -> String {
    let mut buf = [0; 4];
    let path = DESERET.a.encode_utf8(&mut buf).as_bytes().to_owned();
    let mut chars: std::collections::BTreeSet<char> = (0x80..0xC0u8)
        .step_by(2)
        .map(|b4| {
            std::str::from_utf8(&[path[0], path[1], path[2], b4])
                .expect("one UTF-8 character")
                .chars()
                .next()
                .expect("one character")
        })
        .collect();
    chars.extend([DESERET.a, DESERET.b, DESERET.c]);
    format!("[{}]", chars.into_iter().collect::<String>())
}

/// The families the probe measures. Each `matches` needle starts at `a`
/// and ends in a class the corpus never holds, so a DFA state is the set of
/// live positions after the last `a`s: a new state on almost every
/// character, which fills RE2's cache. They vary what makes a state and a
/// program large: the class, a counted repeat, a `\B` assertion, an
/// alternation, a fan class, and a large Unicode class near the batch
/// view's cap. The `extract` needles end in `c`, which every row ends with
/// after its last needle start.
fn families() -> Vec<Family> {
    vec![
        Family {
            name: "a (?:[abc]){k} [^abc]",
            kind: Kind::Matches,
            body: |k| format!("{}(?:{}){{{k}}}{}", DESERET.a, class3(), not3()),
        },
        Family {
            name: "a (?:[abc]{0,2}){k} [^abc]",
            kind: Kind::Matches,
            body: |k| format!("{}(?:{}{{0,2}}){{{k}}}{}", DESERET.a, class3(), not3()),
        },
        Family {
            name: "a (?:\\B[abc]){k} [^abc]",
            kind: Kind::Matches,
            body: |k| format!("{}(?:\\B{}){{{k}}}{}", DESERET.a, class3(), not3()),
        },
        Family {
            name: "a (?:[abc]|[abc][abc]){k} [^abc]",
            kind: Kind::Matches,
            body: |k| {
                let c = class3();
                format!("{}(?:{c}|{c}{c}){{{k}}}{}", DESERET.a, not3())
            },
        },
        Family {
            name: "a (?:FAN32){k} [^abc]",
            kind: Kind::Matches,
            body: |k| format!("{}(?:{}){{{k}}}{}", DESERET.a, fan(), not3()),
        },
        Family {
            name: "a (?:\\pL){k} [^abc]",
            kind: Kind::Matches,
            body: |k| format!("{}(?:\\pL){{{k}}}{}", DESERET.a, not3()),
        },
        Family {
            name: "\\B (?:\\pL?){k} \\PL",
            kind: Kind::Matches,
            body: |k| format!("\\B(?:\\pL?){{{k}}}\\PL"),
        },
        Family {
            name: "extract a (?:[abc]){k} c",
            kind: Kind::Extract,
            body: |k| format!("{}(?:{}){{{k}}}{}", DESERET.a, class3(), DESERET.c),
        },
        Family {
            name: "extract a (?:[abc]{0,2}){k} c",
            kind: Kind::Extract,
            body: |k| format!("{}(?:{}{{0,2}}){{{k}}}{}", DESERET.a, class3(), DESERET.c),
        },
    ]
}

/// The query of `n` distinct copies of `body`. A `matches` copy is told
/// apart by a three-digit suffix, as part 1's `copies` does. An `extract`
/// copy is told apart by its group's name, two groups a stage, because a
/// query holds at most [`MAX_STAGES`] stages: the second group is empty,
/// and its `regexp_extract` still compiles the whole body.
fn dsl(kind: Kind, body: &str, n: usize) -> String {
    match kind {
        Kind::Matches => {
            let list: Vec<String> = (0..n).map(|i| format!("/{body}{i:03}/")).collect();
            format!("message={}", list.join(","))
        }
        Kind::Extract => {
            let stages: Vec<String> = (0..n.div_ceil(2))
                .map(|s| {
                    let second = if 2 * s + 1 < n {
                        format!("(?P<h{s:03}>)")
                    } else {
                        String::new()
                    };
                    format!("extract \"(?P<g{s:03}>{body}){second}\"")
                })
                .collect();
            format!("* | {}", stages.join(" | "))
        }
    }
}

/// The largest count at which 256 copies of the family pass `parse` as one
/// query, and the parse error at the next count. Admission is monotone in
/// the count.
fn edge(f: &Family) -> Option<(usize, String)> {
    let (mut admitted, mut refused, mut why) = (0, 65, String::from("none"));
    while refused - admitted > 1 {
        let k = admitted + (refused - admitted) / 2;
        match parse(&dsl(f.kind, &(f.body)(k), PATTERNS)) {
            Ok(_) => admitted = k,
            Err(errors) => {
                refused = k;
                why = errors
                    .first()
                    .map_or_else(String::new, |e| e.message.clone());
            }
        }
    }
    (admitted > 0).then_some((admitted, why))
}

// ---------------------------------------------------------------------
// The corpus
// ---------------------------------------------------------------------

/// xorshift64, so the corpus is the same on every run.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }

    fn below_usize(&mut self, n: usize) -> usize {
        let n = u64::try_from(n).expect("a small range");
        usize::try_from(self.below(n)).expect("below a usize")
    }
}

/// One row: `len` characters of `x`, `a` at three or eight tenths and the
/// rest `b` and `c`, with no `c` exactly `gap` characters after an `a`.
/// An `extract` row then ends in `a`, `gap - 1` `b`s and `c`: one match,
/// at the end, so the forward scan reads the whole row first.
fn row(rng: &mut Rng, x: Alphabet, len: usize, gap: usize, extract: bool) -> String {
    let a_tenths = if rng.below(2) == 0 { 3 } else { 8 };
    let mut chars: Vec<char> = Vec::with_capacity(len + gap + 1);
    for p in 0..len {
        let mut next = if rng.below(10) < a_tenths {
            x.a
        } else if rng.below(2) == 0 {
            x.c
        } else {
            x.b
        };
        if next == x.c && p >= gap && chars[p - gap] == x.a {
            next = x.b;
        }
        chars.push(next);
    }
    if extract {
        chars.push(x.a);
        chars.extend(std::iter::repeat_n(x.b, gap - 1));
        chars.push(x.c);
    }
    chars.into_iter().collect()
}

/// Shortest and longest row, in characters.
const ROW_CHARS: (usize, usize) = (64, 512);

/// Write [`FILES`] parquet files of `rows_per_file` rows under `dir`, seven
/// in eight over Deseret and the rest over the other alphabets in turn,
/// with `_time` and `message` columns. `gap` is the needle's span, for the
/// `c` rule. Returns the glob and the characters written.
fn write_corpus(dir: &Path, rows_per_file: usize, gap: usize, kind: Kind) -> (String, usize) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).expect("corpus dir");
    let conn = Connection::open_in_memory().expect("in-memory duckdb");
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut chars = 0;
    for file in 0..FILES {
        conn.execute_batch(
            "DROP TABLE IF EXISTS t; CREATE TABLE t (_time TIMESTAMP, message VARCHAR)",
        )
        .expect("create the corpus table");
        {
            let mut app = conn.appender("t").expect("appender");
            for r in 0..rows_per_file {
                let x = if r % 8 == 7 {
                    OTHERS[(r / 8) % OTHERS.len()]
                } else {
                    DESERET
                };
                let len = ROW_CHARS.0 + rng.below_usize(ROW_CHARS.1 - ROW_CHARS.0 + 1);
                let text = row(&mut rng, x, len, gap, kind == Kind::Extract);
                chars += text.chars().count();
                let micros = i64::try_from(file * rows_per_file + r).expect("a time");
                let time = duckdb::types::Value::Timestamp(
                    duckdb::types::TimeUnit::Microsecond,
                    1_767_225_600_000_000 + micros,
                );
                app.append_row(duckdb::params![time, text])
                    .expect("append a row");
            }
        }
        let path = dir.join(format!("part-{file:02}.parquet"));
        conn.execute_batch(&format!("COPY t TO '{}' (FORMAT parquet)", path.display()))
            .expect("write a corpus file");
    }
    (format!("{}/*.parquet", dir.display()), chars)
}

// ---------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------

/// This process's resident memory and its private dirty pages, in bytes.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
struct Mem {
    rss: u64,
    dirty: u64,
}

/// The `kB` figure of the line that starts with `key`, in bytes.
fn kb_field(text: &str, key: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix(key))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse::<u64>().ok())
        .map(|kb| kb * 1024)
}

fn read_mem() -> Mem {
    let status = std::fs::read_to_string("/proc/self/status").expect("read /proc/self/status");
    let rollup =
        std::fs::read_to_string("/proc/self/smaps_rollup").expect("read /proc/self/smaps_rollup");
    Mem {
        rss: kb_field(&status, "VmRSS:").expect("VmRSS"),
        dirty: kb_field(&rollup, "Private_Dirty:").expect("Private_Dirty"),
    }
}

/// The host's load average, for the record beside a measurement.
fn load() -> String {
    std::fs::read_to_string("/proc/loadavg").map_or_else(
        |_| "unknown".to_string(),
        |s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" "),
    )
}

#[expect(clippy::cast_precision_loss, reason = "a MiB figure for a reader")]
fn mib(bytes: i64) -> f64 {
    bytes as f64 / MIB as f64
}

fn signed(bytes: u64) -> i64 {
    i64::try_from(bytes).expect("a byte count")
}

// ---------------------------------------------------------------------
// One measurement, in a child process
// ---------------------------------------------------------------------

/// What a child measures: one point, once.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Spec {
    glob: String,
    /// The measured query, in the DSL.
    dsl: String,
    threads: u32,
    queries: usize,
    memory_limit: u64,
    rss_cap: u64,
    work: PathBuf,
}

/// What a child saw.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Seen {
    base: Mem,
    peak: Mem,
    after: Mem,
    /// `sum(memory_usage_bytes)` of `duckdb_memory()` at the baseline, and
    /// its largest value during the queries.
    duck_base: i64,
    duck_peak: i64,
    secs: f64,
    /// Each query's outcome: `ok`, or its error's text.
    outcomes: Vec<String>,
    /// The watchdog interrupted the queries.
    capped: bool,
}

const SPEC_ENV: &str = "RE2_MEMORY_SPEC";
const SEEN_PREFIX: &str = "RE2_MEMORY_SEEN ";

/// The `duckdb_memory()` total, or -1 when the read fails.
fn duck_total(executor: &Executor) -> i64 {
    executor
        .query_i64_for_test("SELECT sum(memory_usage_bytes)::BIGINT FROM duckdb_memory()")
        .unwrap_or(-1)
}

/// The query database at `scratch`, as trawld opens it, under the spec's
/// threads and memory limit.
fn open_database(spec: &Spec, scratch: &Path) -> Executor {
    let lock = Arc::new(ScratchLock::open(scratch).expect("the scratch directory locks"));
    let budgets = QueryBudgets {
        spill_max_bytes: NonZeroU64::new(4 << 30).expect("non-zero"),
        memory_limit: NonZeroU64::new(spec.memory_limit).expect("non-zero"),
        threads: NonZeroU32::new(spec.threads).expect("non-zero"),
    };
    Executor::open_query_database(lock, &budgets).expect("the query database opens")
}

/// Sample resident memory every 10 ms until `stop`, and interrupt every
/// lane once it grows past `cap` over `base`. Returns the peak.
fn rss_sampler(
    base: Mem,
    cap: u64,
    lanes: &[Executor],
    stop: &Arc<AtomicBool>,
    capped: &Arc<AtomicBool>,
) -> JoinHandle<Mem> {
    let handles: Vec<_> = lanes.iter().map(Executor::interrupt_handle).collect();
    let (stop, capped) = (Arc::clone(stop), Arc::clone(capped));
    thread::spawn(move || {
        let mut peak = base;
        while !stop.load(Ordering::Acquire) {
            let now = read_mem();
            peak.rss = peak.rss.max(now.rss);
            peak.dirty = peak.dirty.max(now.dirty);
            if now.rss.saturating_sub(base.rss) > cap && !capped.swap(true, Ordering::AcqRel) {
                for h in &handles {
                    h.interrupt();
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
        peak
    })
}

/// Sample `duckdb_memory()` every 50 ms on a connection of its own until
/// `stop`. Returns the largest total.
fn duck_sampler(duck: Executor, base: i64, stop: &Arc<AtomicBool>) -> JoinHandle<i64> {
    let stop = Arc::clone(stop);
    thread::spawn(move || {
        let mut peak = base;
        while !stop.load(Ordering::Acquire) {
            peak = peak.max(duck_total(&duck));
            thread::sleep(Duration::from_millis(50));
        }
        peak
    })
}

/// Run the spec's query on every lane at once. Each lane moves into its
/// query's thread and comes back with the outcome, so every connection is
/// still open when the caller reads what is retained: the statements have
/// dropped, the connections have not.
fn run_lanes(spec: &Spec, lanes: Vec<Executor>) -> (Vec<Executor>, Vec<String>) {
    let barrier = Arc::new(Barrier::new(lanes.len()));
    let runs: Vec<_> = lanes
        .into_iter()
        .map(|lane| {
            let barrier = Arc::clone(&barrier);
            let (dsl, glob) = (spec.dsl.clone(), spec.glob.clone());
            thread::spawn(move || {
                barrier.wait();
                let outcome = match lane.run_query(&dsl, &glob, &FieldTypes::new(), usize::MAX, 0) {
                    Ok(result) if result.rows.is_empty() || dsl.starts_with('*') => "ok".to_owned(),
                    Ok(result) => format!("matched {} rows", result.rows.len()),
                    Err(e) => e.to_string(),
                };
                (lane, outcome)
            })
        })
        .collect();
    runs.into_iter()
        .map(|r| r.join().expect("a query thread"))
        .unzip()
}

fn measure(spec: &Spec) -> Seen {
    assert!(
        parse(&spec.dsl).is_ok(),
        "every measured query passes parse"
    );
    let scratch = tempfile::Builder::new()
        .prefix("scratch-")
        .tempdir_in(&spec.work)
        .expect("a scratch directory");
    let root = open_database(spec, scratch.path());
    let lanes: Vec<Executor> = (0..spec.queries)
        .map(|_| root.try_clone().expect("a clone"))
        .collect();
    let duck = root.try_clone().expect("a clone for duckdb_memory()");

    // Warm every lane: read the corpus once through one trivial regex, so
    // the baseline already holds the parquet reader's state.
    for lane in &lanes {
        let warm = lane
            .run_query(
                "message=/zzzz/",
                &spec.glob,
                &FieldTypes::new(),
                usize::MAX,
                0,
            )
            .expect("the warm-up runs");
        assert!(warm.rows.is_empty(), "the warm-up matches nothing");
    }
    thread::sleep(Duration::from_millis(200));
    let base = read_mem();
    let duck_base = duck_total(&duck);

    let stop = Arc::new(AtomicBool::new(false));
    let capped = Arc::new(AtomicBool::new(false));
    let rss = rss_sampler(base, spec.rss_cap, &lanes, &stop, &capped);
    let duckdb = duck_sampler(duck, duck_base, &stop);
    let started = Instant::now();
    let (lanes, outcomes) = run_lanes(spec, lanes);
    let secs = started.elapsed().as_secs_f64();
    thread::sleep(Duration::from_millis(200));
    let after = read_mem();
    stop.store(true, Ordering::Release);
    let peak = rss.join().expect("the sampler");
    let duck_peak = duckdb.join().expect("the duckdb_memory() sampler");
    drop(lanes);
    Seen {
        base,
        peak,
        after,
        duck_base,
        duck_peak,
        secs,
        outcomes,
        capped: capped.load(Ordering::Acquire),
    }
}

/// Run `spec` in a child process: this test binary's `child` test.
fn in_child(spec: &Spec) -> Seen {
    let exe = std::env::current_exe().expect("the test binary");
    let output = Command::new(exe)
        .args([
            "--exact",
            "child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(SPEC_ENV, serde_json::to_string(spec).expect("a spec"))
        .output()
        .expect("the child runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .find_map(|l| l.find(SEEN_PREFIX).map(|at| &l[at + SEEN_PREFIX.len()..]))
        .unwrap_or_else(|| {
            panic!(
                "the child reported nothing; status {}, stderr:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            )
        });
    serde_json::from_str(line).expect("the child's report")
}

/// One measurement, run by a parent part in a process of its own. Without
/// a spec it does nothing.
#[test]
#[ignore = "a child of the measuring parts"]
fn child() {
    let Ok(text) = std::env::var(SPEC_ENV) else {
        return;
    };
    let spec: Spec = serde_json::from_str(&text).expect("the spec");
    let seen = measure(&spec);
    // On a line of its own: the test harness writes `test child ... `
    // before the test's output, with no newline.
    println!(
        "\n{SEEN_PREFIX}{}",
        serde_json::to_string(&seen).expect("a report")
    );
}

// ---------------------------------------------------------------------
// Parts
// ---------------------------------------------------------------------

/// Where the parts keep the corpora, the probe's choice and the results:
/// `RE2_WORK_DIR`, or a directory under the target dir.
fn work_dir() -> PathBuf {
    let dir = std::env::var_os("RE2_WORK_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("re2-memory"),
        PathBuf::from,
    );
    std::fs::create_dir_all(&dir).expect("the work dir");
    dir
}

fn rss_cap() -> u64 {
    std::env::var("RE2_RSS_CAP_MIB")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(RSS_CAP, |m| m * MIB)
}

/// The family the probe chose, as the later parts read it back.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Choice {
    family: String,
    kind: Kind,
    k: usize,
    body: String,
}

const CHOICE_FILE: &str = "choice.json";

fn read_choice(work: &Path) -> Choice {
    let text = std::fs::read_to_string(work.join(CHOICE_FILE))
        .expect("the probe's choice; run the probe part first");
    serde_json::from_str(&text).expect("the choice")
}

/// The sweep's corpus for `choice`, written once into the work directory.
fn sweep_corpus(work: &Path, choice: &Choice) -> (String, usize) {
    let dir = work.join("corpus-sweep");
    let marker = dir.join("chars");
    if let Ok(chars) = std::fs::read_to_string(&marker) {
        let chars = chars.trim().parse().expect("a character count");
        return (format!("{}/*.parquet", dir.display()), chars);
    }
    let (glob, chars) = write_corpus(&dir, SWEEP_ROWS_PER_FILE, choice.k + 1, choice.kind);
    std::fs::write(&marker, chars.to_string()).expect("write the marker");
    (glob, chars)
}

fn header(part: &str, out: &mut String) {
    let _ = writeln!(
        out,
        "part {part}: load average at start (1, 5 and 15 minutes): {}",
        load()
    );
}

fn footer(part: &str, started: Instant, out: &mut String) {
    let _ = writeln!(
        out,
        "part {part}: {:.0} s; load average at end: {}",
        started.elapsed().as_secs_f64(),
        load()
    );
}

/// The growth of one measurement over its baseline: the peak during the
/// queries and what is retained after, for resident memory and private
/// dirty pages, and the peak of `duckdb_memory()`.
#[derive(Clone, Copy, Debug)]
struct Growth {
    peak_rss: i64,
    peak_dirty: i64,
    after_rss: i64,
    after_dirty: i64,
    duck: i64,
}

fn growth(s: &Seen) -> Growth {
    Growth {
        peak_rss: signed(s.peak.rss) - signed(s.base.rss),
        peak_dirty: signed(s.peak.dirty) - signed(s.base.dirty),
        after_rss: signed(s.after.rss) - signed(s.base.rss),
        after_dirty: signed(s.after.dirty) - signed(s.base.dirty),
        duck: s.duck_peak - s.duck_base,
    }
}

/// Which probe families to measure, from `RE2_FAMILIES` (indices into
/// [`families`]) where set, for a run by hand.
fn probe_families() -> Vec<(usize, Family)> {
    let keep: Option<Vec<usize>> = std::env::var("RE2_FAMILIES")
        .ok()
        .map(|list| list.split(',').filter_map(|s| s.parse().ok()).collect());
    families()
        .into_iter()
        .enumerate()
        .filter(|(i, _)| keep.as_ref().is_none_or(|k| k.contains(i)))
        .collect()
}

/// One probe row: a family at its edge, once.
fn probe_row(work: &Path, i: usize, f: &Family, out: &mut String) -> Option<(i64, Choice)> {
    let Some((k, why)) = edge(f) else {
        let _ = writeln!(out, "{:<34} no count is admitted for 256 copies", f.name);
        return None;
    };
    let body = (f.body)(k);
    let (glob, _) = write_corpus(
        &work.join(format!("corpus-probe-{i}")),
        PROBE_ROWS_PER_FILE,
        k + 1,
        f.kind,
    );
    let seen = in_child(&Spec {
        glob,
        dsl: dsl(f.kind, &body, PATTERNS),
        threads: 1,
        queries: 1,
        memory_limit: DEFAULT_QUERY_MEMORY_LIMIT,
        rss_cap: rss_cap(),
        work: work.to_owned(),
    });
    let g = growth(&seen);
    let _ = writeln!(
        out,
        "{:<34} {k:>3} {:>9.1} {:>9.1} {:>9.1} {:>9.1} {:>6.1}  {}",
        f.name,
        mib(g.peak_rss),
        mib(g.peak_dirty),
        mib(g.after_rss),
        mib(g.duck),
        seen.secs,
        outcome_text(&seen),
    );
    let _ = writeln!(out, "{:<34}     next k refused by: {why}", "");
    let choice = Choice {
        family: f.name.to_owned(),
        kind: f.kind,
        k,
        body,
    };
    seen.outcomes
        .iter()
        .all(|o| o == "ok")
        .then_some((g.peak_rss, choice))
}

fn outcome_text(seen: &Seen) -> String {
    let mut text = if seen.outcomes.iter().all(|o| o == "ok") {
        format!("{} ok", seen.outcomes.len())
    } else {
        format!("{:?}", seen.outcomes)
    };
    if seen.capped {
        text.push_str(" (the watchdog interrupted it)");
    }
    text
}

/// The probe: every family at its edge, 256 patterns, one thread, one
/// query, once. Keeps the family whose peak resident growth is largest.
#[test]
#[ignore = "resident-memory evidence for a reader; run through scripts/evidence/issue-335/re2.sh"]
fn probe() {
    let started = Instant::now();
    let mut out = String::new();
    header("probe", &mut out);
    let work = work_dir();
    let version = Connection::open_in_memory()
        .and_then(|c| c.query_row("SELECT version()", [], |r| r.get::<_, String>(0)))
        .expect("the DuckDB version");
    let _ = writeln!(
        out,
        "DuckDB {version}; this host has {} CPUs, so max_concurrent_queries defaults to {}; query_threads defaults to {DEFAULT_QUERY_THREADS} and query_memory_limit to {} MiB",
        thread::available_parallelism().map_or(1, std::num::NonZero::get),
        max_concurrent_queries(),
        DEFAULT_QUERY_MEMORY_LIMIT / MIB
    );
    let _ = writeln!(
        out,
        "corpus: {FILES} parquet files of {PROBE_ROWS_PER_FILE} rows, rows of {} to {} characters, seven in eight Deseret, the rest CJK, ASCII and Specials in turn",
        ROW_CHARS.0, ROW_CHARS.1
    );
    let _ = writeln!(
        out,
        "each family: 256 distinct patterns in one query, at the largest k the parser admits for the whole query; 1 database thread, 1 query, once; growth over the baseline in MiB\n"
    );
    let _ = writeln!(
        out,
        "{:<34} {:>3} {:>9} {:>9} {:>9} {:>9} {:>6}  outcome",
        "family", "k", "peak RSS", "peak dirt", "after RSS", "duckdb_mm", "secs"
    );
    let mut best: Option<(i64, Choice)> = None;
    for (i, f) in probe_families() {
        if let Some((peak, choice)) = probe_row(&work, i, &f, &mut out)
            && best.as_ref().is_none_or(|(b, _)| peak > *b)
        {
            best = Some((peak, choice));
        }
    }
    let (peak, choice) = best.expect("some family runs");
    let _ = writeln!(
        out,
        "\nchosen: {} at k = {}, the largest peak resident growth ({:.1} MiB, {:.2} MiB a pattern); the later parts measure it",
        choice.family,
        choice.k,
        mib(peak),
        mib(peak) / 256.0
    );
    std::fs::write(
        work.join(CHOICE_FILE),
        serde_json::to_string_pretty(&choice).expect("the choice"),
    )
    .expect("write the choice");
    footer("probe", started, &mut out);
    print!("{out}");
}

/// One point of the sweep, measured [`REPS`] times.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Point {
    patterns: usize,
    threads: u32,
    queries: usize,
    memory_limit: u64,
    seen: Vec<Seen>,
}

impl Point {
    fn growths(&self) -> Vec<Growth> {
        self.seen.iter().map(growth).collect()
    }

    /// Median and max of one growth figure across the repeats.
    fn med_max(&self, pick: fn(&Growth) -> i64) -> (i64, i64) {
        let mut xs: Vec<i64> = self.growths().iter().map(pick).collect();
        xs.sort_unstable();
        (xs[xs.len() / 2], xs[xs.len() - 1])
    }

    fn all_ok(&self) -> bool {
        self.seen
            .iter()
            .all(|s| !s.capped && s.outcomes.iter().all(|o| o == "ok"))
    }
}

fn measure_point(
    work: &Path,
    choice: &Choice,
    glob: &str,
    (patterns, threads, queries, memory_limit): (usize, u32, usize, u64),
) -> Point {
    let spec = Spec {
        glob: glob.to_owned(),
        dsl: dsl(choice.kind, &choice.body, patterns),
        threads,
        queries,
        memory_limit,
        rss_cap: rss_cap(),
        work: work.to_owned(),
    };
    Point {
        patterns,
        threads,
        queries,
        memory_limit,
        seen: (0..REPS).map(|_| in_child(&spec)).collect(),
    }
}

fn point_header(out: &mut String) {
    let _ = writeln!(
        out,
        "growth over the baseline in MiB, median / max of {REPS} runs, each in a fresh process\n"
    );
    let _ = writeln!(
        out,
        "{:>8} {:>7} {:>7} {:>9} {:>17} {:>17} {:>17} {:>15} {:>7}  outcome",
        "patterns",
        "threads",
        "queries",
        "mem_limit",
        "peak RSS",
        "peak dirty",
        "after RSS",
        "duckdb_memory",
        "secs"
    );
}

fn point_row(p: &Point, out: &mut String) {
    let pair = |pick: fn(&Growth) -> i64| {
        let (m, x) = p.med_max(pick);
        format!("{:>8.1} / {:>6.1}", mib(m), mib(x))
    };
    let mut secs: Vec<f64> = p.seen.iter().map(|s| s.secs).collect();
    secs.sort_by(f64::total_cmp);
    let outcomes: Vec<String> = p.seen.iter().map(outcome_text).collect();
    let (duck, _) = p.med_max(|g| g.duck);
    let _ = writeln!(
        out,
        "{:>8} {:>7} {:>7} {:>8}M {} {} {} {:>15.1} {:>7.1}  {}",
        p.patterns,
        p.threads,
        p.queries,
        p.memory_limit / MIB,
        pair(|g| g.peak_rss),
        pair(|g| g.peak_dirty),
        pair(|g| g.after_rss),
        mib(duck),
        secs[secs.len() / 2],
        outcomes.join("; ")
    );
}

const RESULTS_PREFIX: &str = "results-";

/// Measure `points` of the chosen family, print them, and keep them for
/// the summary as `results-<part>.json`.
fn sweep_part(part: &str, intro: &str, points: &[(usize, u32, usize, u64)]) {
    let started = Instant::now();
    let mut out = String::new();
    header(part, &mut out);
    let work = work_dir();
    let choice = read_choice(&work);
    let (glob, chars) = sweep_corpus(&work, &choice);
    let _ = writeln!(
        out,
        "{intro}\nfamily {} at k = {}; corpus: {FILES} parquet files of {SWEEP_ROWS_PER_FILE} rows, {chars} characters in all",
        choice.family, choice.k
    );
    point_header(&mut out);
    let mut measured = Vec::new();
    for &point in points {
        let p = measure_point(&work, &choice, &glob, point);
        point_row(&p, &mut out);
        measured.push(p);
    }
    std::fs::write(
        work.join(format!("{RESULTS_PREFIX}{part}.json")),
        serde_json::to_string(&measured).expect("the results"),
    )
    .expect("write the results");
    footer(part, started, &mut out);
    print!("{out}");
}

/// The patterns axis: 1, 64 and 256 patterns in one query, on one thread.
#[test]
#[ignore = "resident-memory evidence for a reader; run through scripts/evidence/issue-335/re2.sh"]
fn patterns() {
    let m = DEFAULT_QUERY_MEMORY_LIMIT;
    sweep_part(
        "patterns",
        "1, 64 and 256 patterns in one query, on one database thread",
        &[(1, 1, 1, m), (64, 1, 1, m), (PATTERNS, 1, 1, m)],
    );
}

/// The threads axis: 256 patterns on 1, 2 and 4 database threads.
#[test]
#[ignore = "resident-memory evidence for a reader; run through scripts/evidence/issue-335/re2.sh"]
fn threads() {
    let m = DEFAULT_QUERY_MEMORY_LIMIT;
    sweep_part(
        "threads",
        "256 patterns in one query, on 1, 2 and 4 database threads",
        &[
            (PATTERNS, 1, 1, m),
            (PATTERNS, 2, 1, m),
            (PATTERNS, 4, 1, m),
        ],
    );
}

/// The concurrency axis: `max_concurrent_queries` queries at once.
#[test]
#[ignore = "resident-memory evidence for a reader; run through scripts/evidence/issue-335/re2.sh"]
fn queries() {
    let (m, q) = (DEFAULT_QUERY_MEMORY_LIMIT, max_concurrent_queries());
    sweep_part(
        "queries",
        &format!(
            "256 patterns, as max_concurrent_queries = {q} queries at once on clones of one database, on 1 and {DEFAULT_QUERY_THREADS} database threads"
        ),
        &[(PATTERNS, 1, q, m), (PATTERNS, DEFAULT_QUERY_THREADS, q, m)],
    );
}

/// The limit: the 256-pattern, 4-thread point under a low `memory_limit`.
#[test]
#[ignore = "resident-memory evidence for a reader; run through scripts/evidence/issue-335/re2.sh"]
fn limit() {
    sweep_part(
        "limit",
        &format!(
            "256 patterns, one query on {DEFAULT_QUERY_THREADS} database threads, with memory_limit at {} MiB, far below RE2's growth: a pass means the limit does not count RE2's memory, and an Out of Memory error means it does",
            LOW_MEMORY_LIMIT / MIB
        ),
        &[(PATTERNS, DEFAULT_QUERY_THREADS, 1, LOW_MEMORY_LIMIT)],
    );
}

fn results(work: &Path, part: &str) -> Vec<Point> {
    let text = std::fs::read_to_string(work.join(format!("{RESULTS_PREFIX}{part}.json")))
        .unwrap_or_else(|_| panic!("the {part} part's results; run it first"));
    serde_json::from_str(&text).expect("the results")
}

fn find(points: &[Point], patterns: usize, threads: u32, queries: usize) -> &Point {
    points
        .iter()
        .find(|p| p.patterns == patterns && p.threads == threads && p.queries == queries)
        .expect("the point was measured")
}

/// The summary: the envelope in plain sentences, from the parts' results.
#[test]
#[ignore = "resident-memory evidence for a reader; run through scripts/evidence/issue-335/re2.sh"]
#[expect(clippy::cast_precision_loss, reason = "MiB figures for a reader")]
fn summary() {
    let work = work_dir();
    let choice = read_choice(&work);
    let by_patterns = results(&work, "patterns");
    let by_threads = results(&work, "threads");
    let by_queries = results(&work, "queries");
    let limited = results(&work, "limit");
    let peak = |p: &Point| p.med_max(|g| g.peak_rss);
    let mut out = String::new();

    let (one, _) = peak(find(&by_patterns, 1, 1, 1));
    let (all, all_max) = peak(find(&by_patterns, PATTERNS, 1, 1));
    let per_pattern = (all - one) as f64 / (PATTERNS - 1) as f64;
    let _ = writeln!(
        out,
        "Per pattern: on one thread, RE2's resident memory grew by {:.1} MiB for 1 pattern and {:.1} MiB (max {:.1}) for 256, so each pattern of {} at k = {} holds about {:.2} MiB while its query runs.",
        mib(one),
        mib(all),
        mib(all_max),
        choice.family,
        choice.k,
        mib(1) * per_pattern
    );

    let (t1, _) = peak(find(&by_threads, PATTERNS, 1, 1));
    let (t4, t4_max) = peak(find(&by_threads, PATTERNS, DEFAULT_QUERY_THREADS, 1));
    let per_thread = (t4 - t1) as f64 / f64::from(DEFAULT_QUERY_THREADS - 1);
    let _ = writeln!(
        out,
        "Per thread: 256 patterns grew by {:.1} MiB on 1 thread and {:.1} MiB (max {:.1}) on {DEFAULT_QUERY_THREADS}, so each further thread adds about {:.1} MiB, or {:.2} MiB for each pattern on each thread: every thread compiles its own copy of every pattern.",
        mib(t1),
        mib(t4),
        mib(t4_max),
        mib(1) * per_thread,
        mib(1) * per_thread / PATTERNS as f64
    );

    let q = max_concurrent_queries();
    let worst = find(&by_queries, PATTERNS, DEFAULT_QUERY_THREADS, q);
    let (w, w_max) = peak(worst);
    let copies = q * DEFAULT_QUERY_THREADS as usize * PATTERNS;
    let projected = mib(1) * per_thread / PATTERNS as f64 * copies as f64;
    if worst.all_ok() {
        let _ = writeln!(
            out,
            "Worst total: {q} queries (max_concurrent_queries) of 256 patterns each, on {DEFAULT_QUERY_THREADS} threads, grew by {:.1} MiB (max {:.1}).",
            mib(w),
            mib(w_max)
        );
    } else {
        let _ = writeln!(
            out,
            "Worst total: {q} queries (max_concurrent_queries) of 256 patterns each, on {DEFAULT_QUERY_THREADS} threads, reached {:.1} MiB (max {:.1}) before the watchdog stopped it at {} MiB of growth. At {:.2} MiB a copy, its {copies} copies (queries x threads x patterns) would hold about {projected:.0} MiB.",
            mib(w),
            mib(w_max),
            rss_cap() / MIB,
            mib(1) * per_thread / PATTERNS as f64
        );
    }
    let (q1, _) = peak(find(&by_queries, PATTERNS, 1, q));
    let _ = writeln!(
        out,
        "On 1 thread, {q} queries at once grew by {:.1} MiB, against {:.1} MiB for one: each query holds its own copies.",
        mib(q1),
        mib(t1)
    );

    let low = &limited[0];
    let (l, _) = peak(low);
    let (duck, _) = find(&by_threads, PATTERNS, DEFAULT_QUERY_THREADS, 1).med_max(|g| g.duck);
    let verdict = if low.all_ok() {
        format!(
            "every run passed while RE2 grew by {:.1} MiB, so query_memory_limit does not account for RE2's memory",
            mib(l)
        )
    } else {
        let outcomes: Vec<String> = low.seen.iter().map(outcome_text).collect();
        format!(
            "the runs ended {}, so query_memory_limit does account for RE2's memory",
            outcomes.join("; ")
        )
    };
    let _ = writeln!(
        out,
        "Memory limit: under memory_limit = {} MiB, {verdict}. duckdb_memory() grew by {:.1} MiB at the 256-pattern, {DEFAULT_QUERY_THREADS}-thread point.",
        LOW_MEMORY_LIMIT / MIB,
        mib(duck)
    );
    let (after, _) = find(&by_threads, PATTERNS, DEFAULT_QUERY_THREADS, 1).med_max(|g| g.after_rss);
    let (after_dirty, _) =
        find(&by_threads, PATTERNS, DEFAULT_QUERY_THREADS, 1).med_max(|g| g.after_dirty);
    let _ = writeln!(
        out,
        "Retained: once the statements dropped, {:.1} MiB of resident memory ({:.1} MiB private dirty) stayed over the baseline at that point, with the connections still open.",
        mib(after),
        mib(after_dirty)
    );
    print!("{out}");
}

#[test]
fn reads_this_process_memory() {
    let mem = read_mem();
    assert!(mem.rss > 0 && mem.dirty > 0, "{mem:?}");
}

#[test]
fn every_family_is_admitted_at_its_edge() {
    for f in families() {
        let (k, _) = edge(&f).unwrap_or_else(|| {
            let one = parse(&dsl(f.kind, &(f.body)(1), PATTERNS)).err();
            panic!("{} has an admitted count: {one:?}", f.name)
        });
        assert!(
            parse(&dsl(f.kind, &(f.body)(k), PATTERNS)).is_ok(),
            "{}",
            f.name
        );
        assert!(
            k == 64 || parse(&dsl(f.kind, &(f.body)(k + 1), PATTERNS)).is_err(),
            "{}",
            f.name
        );
    }
}

#[test]
fn an_extract_query_spends_the_pattern_cap_within_the_stage_limit() {
    let body = format!("{}{}", DESERET.a, DESERET.c);
    let q = dsl(Kind::Extract, &body, PATTERNS);
    assert_eq!(q.matches("extract ").count(), MAX_STAGES);
    assert_eq!(q.matches("(?P<").count(), PATTERNS);
    parse(&q).expect("256 named groups in 128 stages are admitted");
    assert_eq!(dsl(Kind::Extract, &body, 1).matches("(?P<").count(), 1);
}
