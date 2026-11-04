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
//! Every query is measured beside a control: the same query shape over the
//! same rows, with the same output, and a body whose DFA has at most
//! `k + 2` states (`a`, `k` literal `b`s, and the needle's end), so RE2
//! holds next to nothing for it. RE2's share is a run's growth over the
//! control run beside it. The control matters for `extract`: 128 stages of
//! it grow `DuckDB`'s own buffers by about a GiB, which is not RE2's.
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
//! 256 bytes each on average. Every family runs beside its control, and
//! the probe keeps the `matches` family whose RE2 share is largest, at one
//! thread, for the later parts. It records the largest `extract` share as
//! well, but does not sweep it: 128 `extract` stages grow `DuckDB`'s own
//! buffers by about a GiB, and the control's growth varies by hundreds of
//! MiB from run to run, so an `extract` share is rough. The summary scales
//! the worst total by the `extract` share where it is the larger.
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
//!   that still passes, beside a control that passes, holds RE2 memory the
//!   limit does not count.
//!
//! Each part writes its results into the work directory, and `summary`
//! reads them back and states the envelope.
//!
//! # The allowance per site
//!
//! Since 2026-11-04 every batch query reserves an RE2 allowance per
//! regex site per thread from the regex reservation (human ruling). The
//! sweep's bodies are about 256 bytes, because the query cap leaves no
//! room for more at 256 copies, so two more parts measure single bodies
//! at the full 1024-byte body limit:
//!
//! - `site_size`: each of [`site_families`], padded to the body limit, as
//!   1, 4 and 16 copies on 1 and 4 threads, each beside a control of the
//!   same shape. It checks that one `regexp_` call runs a site.
//! - `allowance`: measures the ordinary bodies as 16 copies on 4 threads,
//!   then fits a flat allowance, one in the body's batch-view size, and
//!   one by function, each at least [`MARGIN_PERCENT`] % over every point
//!   it covers, and states what each charges ordinary queries.
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

/// The control for a needle of `kind` at count `k`: the same query shape
/// over the same rows, with the same output, but a body whose DFA has at
/// most `k + 2` states, so RE2 holds next to nothing for it: `a`, `k`
/// literal `b`s, and the needle's end. An `extract` control matches each
/// row's tail, as the needle does. What a query grows by beyond its
/// control is RE2's: the 128-stage `extract` query grows `DuckDB`'s own
/// buffers by far more than its regexes hold.
fn control(kind: Kind, k: usize) -> String {
    let end = match kind {
        Kind::Matches => not3(),
        Kind::Extract => DESERET.c.to_string(),
    };
    format!("{}{}{{{k}}}{end}", DESERET.a, DESERET.b)
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

#[expect(clippy::cast_precision_loss, reason = "a KiB figure for a reader")]
fn kib(bytes: usize) -> f64 {
    bytes as f64 / 1024.0
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
    control: String,
}

/// The probe's choice for the sweep: the `matches` family with the largest
/// RE2 share.
const CHOICE_FILE: &str = "choice.json";

/// What the probe found for both kinds, for the summary.
const PROBE_FILE: &str = "probe.json";

/// The probe's shares: the chosen `matches` family's, and the largest
/// `extract` family's with its name and count.
///
/// The sweep runs the `matches` family because an `extract` control's own
/// growth varies by hundreds of MiB from run to run: 128 `extract` stages
/// grow `DuckDB`'s buffers by about a GiB. So the `extract` share is known
/// only to within that, and the summary carries the larger of the two
/// shares into the worst total.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ProbeNote {
    matches_share: i64,
    extract: Option<(String, usize, i64)>,
}

fn read_choice(work: &Path, file: &str) -> Choice {
    let text = std::fs::read_to_string(work.join(file))
        .expect("the probe's choice; run the probe part first");
    serde_json::from_str(&text).expect("the choice")
}

/// The sweep's corpus for `choice`, written once into the work directory.
fn sweep_corpus(work: &Path, choice: &Choice) -> (String, usize) {
    let dir = work.join(format!("corpus-sweep-{:?}-{}", choice.kind, choice.k));
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
/// queries, of resident memory and of private dirty pages, the resident
/// memory retained after, and the peak of `duckdb_memory()`.
#[derive(Clone, Copy, Debug)]
struct Growth {
    peak_rss: i64,
    peak_dirty: i64,
    after_rss: i64,
    duck: i64,
}

fn growth(s: &Seen) -> Growth {
    Growth {
        peak_rss: signed(s.peak.rss) - signed(s.base.rss),
        peak_dirty: signed(s.peak.dirty) - signed(s.base.dirty),
        after_rss: signed(s.after.rss) - signed(s.base.rss),
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

/// One probe row: a family at its edge, and its control, once each.
/// Returns RE2's share of the peak growth, the family's over the control's.
fn probe_row(
    work: &Path,
    index: usize,
    family: &Family,
    out: &mut String,
) -> Option<(i64, Choice)> {
    let Some((k, why)) = edge(family) else {
        let _ = writeln!(
            out,
            "{:<34} no count is admitted for 256 copies",
            family.name
        );
        return None;
    };
    let choice = Choice {
        family: family.name.to_owned(),
        kind: family.kind,
        k,
        body: (family.body)(k),
        control: control(family.kind, k),
    };
    let (glob, _) = write_corpus(
        &work.join(format!("corpus-probe-{index}")),
        PROBE_ROWS_PER_FILE,
        k + 1,
        family.kind,
    );
    let spec = |body: &str| Spec {
        glob: glob.clone(),
        dsl: dsl(family.kind, body, PATTERNS),
        threads: 1,
        queries: 1,
        memory_limit: DEFAULT_QUERY_MEMORY_LIMIT,
        rss_cap: rss_cap(),
        work: work.to_owned(),
    };
    let seen = in_child(&spec(&choice.body));
    let base = in_child(&spec(&choice.control));
    let (fam, ctl) = (growth(&seen), growth(&base));
    let share = fam.peak_rss - ctl.peak_rss;
    let _ = writeln!(
        out,
        "{:<34} {k:>3} {:>9.1} {:>9.1} {:>9.1} {:>9.1} {:>9.1} {:>6.1}  {}",
        family.name,
        mib(fam.peak_rss),
        mib(ctl.peak_rss),
        mib(share),
        mib(fam.after_rss),
        mib(fam.duck),
        seen.secs,
        outcome_text(&seen),
    );
    let _ = writeln!(
        out,
        "{:<34}     control {}: {}; next k refused by: {why}",
        "",
        choice.control,
        outcome_text(&base)
    );
    (all_ok(std::slice::from_ref(&seen)) && all_ok(std::slice::from_ref(&base)))
        .then_some((share, choice))
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
/// query, once, beside its control. Keeps the `matches` family with the
/// largest RE2 share of the peak for the sweep, and notes the largest
/// `extract` share for the summary.
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
        "each family: 256 distinct patterns in one query, at the largest k the parser admits for the whole query, and its control (a k b's end: the same query shape and output, a DFA of at most k + 2 states); 1 database thread, 1 query, once each; growth over the baseline in MiB; RE2's share is the family's peak over its control's\n"
    );
    let _ = writeln!(
        out,
        "{:<34} {:>3} {:>9} {:>9} {:>9} {:>9} {:>9} {:>6}  outcome",
        "family", "k", "peak RSS", "control", "RE2 share", "after RSS", "duckdb_mm", "secs"
    );
    let mut best_matches: Option<(i64, Choice)> = None;
    let mut best_extract: Option<(i64, Choice)> = None;
    for (index, family) in probe_families() {
        let Some((share, choice)) = probe_row(&work, index, &family, &mut out) else {
            continue;
        };
        let best = match choice.kind {
            Kind::Matches => &mut best_matches,
            Kind::Extract => &mut best_extract,
        };
        if best.as_ref().is_none_or(|(b, _)| share > *b) {
            *best = Some((share, choice));
        }
    }
    let (share, choice) = best_matches.expect("some matches family runs");
    let _ = writeln!(
        out,
        "\nchosen for the sweep: {} at k = {}, the matches family with the largest RE2 share ({:.1} MiB, {:.2} MiB a pattern)",
        choice.family,
        choice.k,
        mib(share),
        mib(share) / 256.0
    );
    if let Some((extract, e)) = &best_extract {
        let _ = writeln!(
            out,
            "the largest extract share: {} at k = {} ({:.1} MiB, {:.2} MiB a pattern). An extract control's own growth varies by hundreds of MiB from run to run, because 128 extract stages grow DuckDB's buffers by about a GiB, so the sweep measures the matches family, and the summary carries the larger of the two shares into the worst total.",
            e.family,
            e.k,
            mib(*extract),
            mib(*extract) / 256.0
        );
    }
    let note = ProbeNote {
        matches_share: share,
        extract: best_extract.map(|(x, e)| (e.family, e.k, x)),
    };
    std::fs::write(
        work.join(PROBE_FILE),
        serde_json::to_string_pretty(&note).expect("the probe note"),
    )
    .expect("write the probe note");
    std::fs::write(
        work.join(CHOICE_FILE),
        serde_json::to_string_pretty(&choice).expect("the choice"),
    )
    .expect("write the choice");
    footer("probe", started, &mut out);
    print!("{out}");
}

/// One point of the sweep: the chosen family and its control, each
/// measured [`REPS`] times, in turns.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Point {
    patterns: usize,
    threads: u32,
    queries: usize,
    memory_limit: u64,
    seen: Vec<Seen>,
    control: Vec<Seen>,
}

/// Median and max.
fn med_max(mut xs: Vec<i64>) -> (i64, i64) {
    xs.sort_unstable();
    (xs[xs.len() / 2], xs[xs.len() - 1])
}

fn all_ok(seen: &[Seen]) -> bool {
    seen.iter()
        .all(|s| !s.capped && s.outcomes.iter().all(|o| o == "ok"))
}

impl Point {
    /// Median and max of one growth figure of the family's runs.
    fn family(&self, pick: fn(&Growth) -> i64) -> (i64, i64) {
        med_max(self.seen.iter().map(|s| pick(&growth(s))).collect())
    }

    /// Median and max of one growth figure of the control's runs.
    fn control(&self, pick: fn(&Growth) -> i64) -> (i64, i64) {
        med_max(self.control.iter().map(|s| pick(&growth(s))).collect())
    }

    /// RE2's share of one growth figure: each family run over the control
    /// run beside it, median and max.
    fn share(&self, pick: fn(&Growth) -> i64) -> (i64, i64) {
        med_max(
            self.seen
                .iter()
                .zip(&self.control)
                .map(|(f, c)| pick(&growth(f)) - pick(&growth(c)))
                .collect(),
        )
    }
}

fn measure_point(
    work: &Path,
    choice: &Choice,
    glob: &str,
    (patterns, threads, queries, memory_limit): (usize, u32, usize, u64),
) -> Point {
    let spec = |body: &str| Spec {
        glob: glob.to_owned(),
        dsl: dsl(choice.kind, body, patterns),
        threads,
        queries,
        memory_limit,
        rss_cap: rss_cap(),
        work: work.to_owned(),
    };
    let (family, control) = (spec(&choice.body), spec(&choice.control));
    let (mut seen, mut base) = (Vec::new(), Vec::new());
    for _ in 0..REPS {
        seen.push(in_child(&family));
        base.push(in_child(&control));
    }
    Point {
        patterns,
        threads,
        queries,
        memory_limit,
        seen,
        control: base,
    }
}

fn point_header(out: &mut String) {
    let _ = writeln!(
        out,
        "growth over the baseline in MiB, median / max of {REPS} runs, each in a fresh process; the control is the same query with a body RE2 holds next to nothing for, and RE2's share is each run over the control run beside it\n"
    );
    let _ = writeln!(
        out,
        "{:>8} {:>7} {:>7} {:>9} {:>17} {:>9} {:>17} {:>10} {:>9} {:>9} {:>7}  outcome",
        "patterns",
        "threads",
        "queries",
        "mem_limit",
        "peak RSS",
        "control",
        "RE2 share",
        "share drt",
        "after RSS",
        "duckdb_mm",
        "secs"
    );
}

fn point_row(p: &Point, out: &mut String) {
    let pair = |(m, x): (i64, i64)| format!("{:>8.1} / {:>6.1}", mib(m), mib(x));
    let mut secs: Vec<f64> = p.seen.iter().map(|s| s.secs).collect();
    secs.sort_by(f64::total_cmp);
    let mut outcome: Vec<String> = p.seen.iter().map(outcome_text).collect();
    if !all_ok(&p.control) {
        let control: Vec<String> = p.control.iter().map(outcome_text).collect();
        outcome.push(format!("control: {}", control.join("; ")));
    }
    let _ = writeln!(
        out,
        "{:>8} {:>7} {:>7} {:>8}M {} {:>9.1} {} {:>10.1} {:>9.1} {:>9.1} {:>7.1}  {}",
        p.patterns,
        p.threads,
        p.queries,
        p.memory_limit / MIB,
        pair(p.family(|g| g.peak_rss)),
        mib(p.control(|g| g.peak_rss).0),
        pair(p.share(|g| g.peak_rss)),
        mib(p.share(|g| g.peak_dirty).0),
        mib(p.family(|g| g.after_rss).0),
        mib(p.family(|g| g.duck).0),
        secs[secs.len() / 2],
        outcome.join("; ")
    );
}

const RESULTS_PREFIX: &str = "results-";

/// Measure `points` of the family in `choice_file`, print them, and keep
/// them for the summary as `results-<key>.json`.
fn sweep_part(
    (part, key, choice_file): (&str, &str, &str),
    intro: &str,
    points: &[(usize, u32, usize, u64)],
) {
    let started = Instant::now();
    let mut out = String::new();
    header(part, &mut out);
    let work = work_dir();
    let choice = read_choice(&work, choice_file);
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
        work.join(format!("{RESULTS_PREFIX}{key}.json")),
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
        ("patterns", "patterns", CHOICE_FILE),
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
        ("threads", "threads", CHOICE_FILE),
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
        ("queries", "queries", CHOICE_FILE),
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
    let intro = format!(
        "256 patterns, one query on {DEFAULT_QUERY_THREADS} database threads, with memory_limit at {} MiB, far below RE2's growth: a pass means the limit does not count RE2's memory, and an Out of Memory error while the control passes means it does",
        LOW_MEMORY_LIMIT / MIB
    );
    let point = [(PATTERNS, DEFAULT_QUERY_THREADS, 1, LOW_MEMORY_LIMIT)];
    sweep_part(("limit", "limit", CHOICE_FILE), &intro, &point);
}

/// What a run under the low `memory_limit` says about RE2's memory.
fn limit_verdict(low: &Point, choice: &Choice) -> String {
    let what = format!("{} at k = {}", choice.family, choice.k);
    match (all_ok(&low.seen), all_ok(&low.control)) {
        (true, _) => format!(
            "every run of {what} passed while it grew by {:.1} MiB ({:.1} MiB over its control), so query_memory_limit does not account for RE2's memory",
            mib(low.family(|g| g.peak_rss).0),
            mib(low.share(|g| g.peak_rss).0)
        ),
        (false, true) => {
            let outcomes: Vec<String> = low.seen.iter().map(outcome_text).collect();
            format!(
                "the runs of {what} ended {} while the control passed, so query_memory_limit does account for RE2's memory",
                outcomes.join("; ")
            )
        }
        (false, false) => {
            let outcomes: Vec<String> = low.control.iter().map(outcome_text).collect();
            format!(
                "the control of {what} failed too ({}), so its run cannot tell",
                outcomes.join("; ")
            )
        }
    }
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

/// The worst total scaled by how much more the probe's largest `extract`
/// family held than the sweep's family, when it held more.
#[expect(clippy::cast_precision_loss, reason = "MiB figures for a reader")]
fn extract_scaling(work: &Path, total: f64, out: &mut String) {
    let note: ProbeNote = serde_json::from_str(
        &std::fs::read_to_string(work.join(PROBE_FILE)).expect("the probe note"),
    )
    .expect("the probe note");
    if let Some((family, k, extract)) = note.extract
        && extract > note.matches_share
    {
        let factor = extract as f64 / note.matches_share as f64;
        let _ = writeln!(
            out,
            "In the probe, 256 patterns of {family} at k = {k} held {:.1} MiB over their control, {factor:.2} times the {:.1} MiB of the sweep's family; that control's own growth varies by hundreds of MiB, so the figure is rough. Scaled by it, the worst total is about {:.0} MiB.",
            mib(extract),
            mib(note.matches_share),
            total * factor
        );
    } else {
        let _ = writeln!(
            out,
            "In the probe, no extract family held more than the sweep's family, so the worst total stands."
        );
    }
}

/// The summary: the envelope in plain sentences, from the parts' results.
#[test]
#[ignore = "resident-memory evidence for a reader; run through scripts/evidence/issue-335/re2.sh"]
#[expect(clippy::cast_precision_loss, reason = "MiB figures for a reader")]
fn summary() {
    let work = work_dir();
    let choice = read_choice(&work, CHOICE_FILE);
    let by_patterns = results(&work, "patterns");
    let by_threads = results(&work, "threads");
    let by_queries = results(&work, "queries");
    let limited = results(&work, "limit");
    let share = |p: &Point| p.share(|g| g.peak_rss);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "RE2's share is a query's peak resident growth over its control's: the same query shape and output with a body RE2 holds next to nothing for. Medians of {REPS} runs, max in brackets."
    );

    let (one, _) = share(find(&by_patterns, 1, 1, 1));
    let (all, all_max) = share(find(&by_patterns, PATTERNS, 1, 1));
    let per_pattern = (all - one) as f64 / (PATTERNS - 1) as f64;
    let _ = writeln!(
        out,
        "Per pattern: on one thread, RE2's share was {:.1} MiB for 1 pattern and {:.1} MiB ({:.1}) for 256, so each pattern of {} at k = {} holds about {:.2} MiB while its query runs.",
        mib(one),
        mib(all),
        mib(all_max),
        choice.family,
        choice.k,
        mib(1) * per_pattern
    );

    let (t1, _) = share(find(&by_threads, PATTERNS, 1, 1));
    let t4_point = find(&by_threads, PATTERNS, DEFAULT_QUERY_THREADS, 1);
    let (t4, t4_max) = share(t4_point);
    let per_thread = (t4 - t1) as f64 / f64::from(DEFAULT_QUERY_THREADS - 1);
    let per_copy = mib(1) * per_thread / PATTERNS as f64;
    let _ = writeln!(
        out,
        "Per thread: for 256 patterns RE2's share was {:.1} MiB on 1 thread and {:.1} MiB ({:.1}) on {DEFAULT_QUERY_THREADS}, so each further thread adds about {:.1} MiB, or {per_copy:.2} MiB for each pattern on each thread.",
        mib(t1),
        mib(t4),
        mib(t4_max),
        mib(1) * per_thread,
    );

    let q = max_concurrent_queries();
    let worst = find(&by_queries, PATTERNS, DEFAULT_QUERY_THREADS, q);
    let (w, w_max) = share(worst);
    let copies = q * DEFAULT_QUERY_THREADS as usize * PATTERNS;
    let total = if all_ok(&worst.seen) {
        let _ = writeln!(
            out,
            "Worst total: {q} queries at once (max_concurrent_queries), each of 256 patterns, on {DEFAULT_QUERY_THREADS} threads: RE2's share was {:.1} MiB ({:.1}). That is about {:.0} patterns' worth at the one-thread figure above, under the {copies} copies (queries x threads x patterns) the queries could hold at most, because the {q} queries share the database's {DEFAULT_QUERY_THREADS} threads.",
            mib(w),
            mib(w_max),
            w as f64 / per_pattern
        );
        mib(w_max)
    } else {
        let (peak, _) = worst.family(|g| g.peak_rss);
        let projected = per_copy * copies as f64;
        let _ = writeln!(
            out,
            "Worst total: {q} queries at once (max_concurrent_queries), each of 256 patterns, on {DEFAULT_QUERY_THREADS} threads, grew by {:.1} MiB before the watchdog interrupted them at {} MiB of growth, so the measured envelope is at least that. At {per_copy:.2} MiB for each pattern on each thread, the {copies} copies (queries x threads x patterns) they could hold at most would hold about {projected:.0} MiB.",
            mib(peak),
            rss_cap() / MIB,
        );
        projected.max(mib(peak))
    };
    extract_scaling(&work, total, &mut out);
    let (q1, _) = share(find(&by_queries, PATTERNS, 1, q));
    let _ = writeln!(
        out,
        "On 1 thread, {q} queries at once gave RE2 a share of {:.1} MiB, against {:.1} MiB for one query: each query holds its own copies.",
        mib(q1),
        mib(t1)
    );

    let verdict = limit_verdict(&limited[0], &choice);
    let _ = writeln!(
        out,
        "Memory limit: under memory_limit = {} MiB, {verdict}. At the 256-pattern, {DEFAULT_QUERY_THREADS}-thread point duckdb_memory() grew by {:.1} MiB, and by {:.1} MiB for the control.",
        LOW_MEMORY_LIMIT / MIB,
        mib(t4_point.family(|g| g.duck).0),
        mib(t4_point.control(|g| g.duck).0)
    );
    let _ = writeln!(
        out,
        "Retained: once the statements dropped, with the connections still open, {:.1} MiB of resident memory stayed over the baseline at that point, and {:.1} MiB for the control.",
        mib(t4_point.family(|g| g.after_rss).0),
        mib(t4_point.control(|g| g.after_rss).0)
    );
    print!("{out}");
}

// ---------------------------------------------------------------------
// The allowance per site: single bodies at the full body limit
// ---------------------------------------------------------------------

/// The site counts the `site_size` part runs: copies of one body in one
/// query.
const SITE_COUNTS: [usize; 3] = [1, 4, 16];

/// The database threads the `site_size` part runs each count on.
const SITE_THREADS: [u32; 2] = [1, DEFAULT_QUERY_THREADS];

/// The sites an ordinary body runs as: enough copies that its share
/// rises above the run-to-run noise.
const ORDINARY_SITES: usize = 16;

/// The margin the allowance model keeps over the worst measured point:
/// 25 %, as a fraction in hundredths.
const MARGIN_PERCENT: u64 = 25;

/// The concurrent ordinary queries the stop rule asks a reservation to
/// hold, and the reservation it asks for at most.
const STOP_QUERIES: u64 = 16;
const STOP_RESERVATION: u64 = 1 << 30;

/// A filler that no corpus row holds: `q` is in none of the alphabets.
const PAD: char = 'q';

/// One body the `site_size` part measures at the full body limit.
struct SiteFamily {
    name: &'static str,
    kind: Kind,
    /// The core at count `k`; the part pads it to the body limit.
    core: fn(usize) -> String,
    /// Whether to pad the core with a never-matching literal branch, as
    /// `(?:core|qqq…)`, up to the largest length `parse` admits.
    pad: bool,
}

/// The bodies that might hold the most: each core is a DFA that meets a
/// new state on almost every character, at the largest count `parse`
/// admits for 16 copies, padded to the 1024-byte body limit. They vary
/// what makes a state and a program large: an alternation, a counted
/// repeat near 64 positions, a fan class, a large Unicode class at the
/// batch view's cap, and `(?i)`. CP8's own 256-copy body (unpadded)
/// shows what a short body holds. The `extract` bodies run RE2's reverse
/// program too: the first blows up forward, the second also backward,
/// from the match's end.
fn site_families() -> Vec<SiteFamily> {
    vec![
        SiteFamily {
            name: "alt short: a (?:C|CC){k} N",
            kind: Kind::Matches,
            core: |k| {
                let c = class3();
                format!("{}(?:{c}|{c}{c}){{{k}}}{}", DESERET.a, not3())
            },
            pad: false,
        },
        SiteFamily {
            name: "alt: a (?:C|CC){k} N",
            kind: Kind::Matches,
            core: |k| {
                let c = class3();
                format!("{}(?:{c}|{c}{c}){{{k}}}{}", DESERET.a, not3())
            },
            pad: true,
        },
        SiteFamily {
            name: "repeat: a (?:C){k} N",
            kind: Kind::Matches,
            core: |k| format!("{}(?:{}){{{k}}}{}", DESERET.a, class3(), not3()),
            pad: true,
        },
        SiteFamily {
            name: "fan: a (?:FAN32){k} N",
            kind: Kind::Matches,
            core: |k| format!("{}(?:{}){{{k}}}{}", DESERET.a, fan(), not3()),
            pad: true,
        },
        SiteFamily {
            name: "unicode: a (?:\\pL){k} N",
            kind: Kind::Matches,
            core: |k| format!("{}(?:\\pL){{{k}}}{}", DESERET.a, not3()),
            pad: true,
        },
        SiteFamily {
            name: "(?i) alt: a (?:C|CC){k} N",
            kind: Kind::Matches,
            core: |k| {
                let c = class3();
                format!("(?i){}(?:{c}|{c}{c}){{{k}}}{}", DESERET.a, not3())
            },
            pad: true,
        },
        SiteFamily {
            name: "extract fwd: a (?:C){k} c",
            kind: Kind::Extract,
            core: |k| format!("{}(?:{}){{{k}}}{}", DESERET.a, class3(), DESERET.c),
            pad: true,
        },
        SiteFamily {
            name: "extract fwd+rev: a C{k} c | C{k} a C* c",
            kind: Kind::Extract,
            core: |k| {
                let c = class3();
                format!(
                    "{a}(?:{c}){{{k}}}{e}|(?:{c}){{{k}}}{a}{c}*{e}",
                    a = DESERET.a,
                    e = DESERET.c
                )
            },
            pad: true,
        },
    ]
}

/// The ordinary bodies: the log-pattern corpus the charge is calibrated
/// over, and `error|warn`.
fn ordinary_bodies() -> Vec<String> {
    trawl_core::pattern::charge::LOG_REGEXES
        .iter()
        .map(|&b| b.to_owned())
        .chain(std::iter::once("error|warn".to_owned()))
        .collect()
}

/// `core` padded: `(?:core|q…)` with `pad` filler characters, or `core`
/// itself at none.
fn padded(core: &str, pad: usize) -> String {
    if pad == 0 {
        core.to_owned()
    } else {
        format!("(?:{core}|{})", PAD.to_string().repeat(pad))
    }
}

/// The largest `n` in `0..=max` for which `admits(n)`, which is monotone
/// (true up to some `n`, false after), or `None` when `admits(0)` fails.
fn largest(max: usize, admits: impl Fn(usize) -> bool) -> Option<usize> {
    if !admits(0) {
        return None;
    }
    let (mut ok, mut bad) = (0, max + 1);
    while bad - ok > 1 {
        let mid = ok + (bad - ok) / 2;
        if admits(mid) {
            ok = mid;
        } else {
            bad = mid;
        }
    }
    Some(ok)
}

/// A family's body at its edge: the largest count whose core passes
/// `parse` as 16 copies, then, for a padded family, the largest padding
/// that still does. Returns the count and the body.
fn site_body(f: &SiteFamily) -> Option<(usize, String)> {
    let n = SITE_COUNTS[SITE_COUNTS.len() - 1];
    let parses = |body: &str| parse(&dsl(f.kind, body, n)).is_ok();
    let k = largest(64, |k| k == 0 || parses(&(f.core)(k)))?;
    if k == 0 {
        return None;
    }
    let core = (f.core)(k);
    let pad = if f.pad {
        largest(trawl_core::pattern::MAX_REGEX_LEN, |p| {
            parses(&padded(&core, p))
        })?
    } else {
        0
    };
    Some((k, padded(&core, pad)))
}

/// The regex text one site of `body` binds: what the first copy of the
/// query hands RE2.
fn bound_text(kind: Kind, body: &str) -> String {
    match kind {
        Kind::Matches => format!("{body}000"),
        Kind::Extract => format!("(?P<g000>{body})"),
    }
}

/// The batch view's compiled size of the text one site binds, as the
/// ledger measures it under its 512 KiB cap.
fn view_bytes(kind: Kind, body: &str) -> usize {
    trawl_core::pattern::Preparer::new()
        .batch_compiled_bytes(&bound_text(kind, body))
        .expect("an admitted body measures")
}

/// How many `regexp_*` calls `DuckDB`'s logical plan of the SQL the
/// emitter writes for `dsl` over `glob` holds, unoptimized or optimized.
///
/// The optimized count is the sites that run, each compiling its own RE2
/// on every thread. The unoptimized count is what `DuckDB` binds:
/// `nullif(regexp_extract(…), '')` binds as a `CASE` that holds the call
/// twice, and the optimizer's common-subexpression pass folds the two
/// into one. (`tests/integration.rs`'s `bound_regex_calls` counts
/// `regexp_matches` alone, unoptimized.)
fn bound_sites(dsl: &str, glob: &str, optimize: bool) -> u64 {
    fn count(node: &serde_json::Value) -> u64 {
        match node {
            serde_json::Value::Object(map) => {
                let own = u64::from(
                    map.get("name")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|n| n.starts_with("regexp_")),
                );
                own + map.values().map(count).sum::<u64>()
            }
            serde_json::Value::Array(items) => items.iter().map(count).sum(),
            _ => 0,
        }
    }
    let query = parse(dsl).expect("the query parses");
    let emitted =
        trawl_core::emitter::emit(&query, glob, trawl_core::context::EvalContext::capture())
            .expect("the query emits");
    let conn = Connection::open_in_memory().expect("in-memory duckdb");
    let plan: String = conn
        .query_row(
            "SELECT json_serialize_plan(?::VARCHAR, skip_null := true, skip_empty := true, \
             optimize := ?)",
            duckdb::params![emitted.sql, optimize],
            |row| row.get(0),
        )
        .expect("the plan serializes");
    let plan: serde_json::Value = serde_json::from_str(&plan).expect("the plan is JSON");
    assert_ne!(
        plan.get("error"),
        Some(&serde_json::Value::Bool(true)),
        "{dsl} must bind: {plan}"
    );
    count(&plan)
}

/// One measured body of the `site_size` part.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct SiteBody {
    name: String,
    kind: Kind,
    ordinary: bool,
    k: usize,
    body: String,
    view: usize,
    points: Vec<SitePoint>,
}

/// One body as `sites` copies on `threads` threads, [`REPS`] runs.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct SitePoint {
    sites: usize,
    threads: u32,
    seen: Vec<Seen>,
}

/// The shared control of one shape: `sites` copies of the control body
/// of `kind`, on `threads` threads, [`REPS`] runs.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct SiteControl {
    kind: Kind,
    sites: usize,
    threads: u32,
    seen: Vec<Seen>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SiteResults {
    control_k: usize,
    controls: Vec<SiteControl>,
    bodies: Vec<SiteBody>,
}

const SITE_RESULTS: &str = "results-site_size.json";

impl SiteResults {
    fn control(&self, kind: Kind, sites: usize, threads: u32) -> &SiteControl {
        self.controls
            .iter()
            .find(|c| c.kind == kind && c.sites == sites && c.threads == threads)
            .expect("the control was measured")
    }

    /// RE2's share at `p` of `b`: each run's peak growth over the control
    /// run beside it, median and max.
    fn share(&self, b: &SiteBody, p: &SitePoint) -> (i64, i64) {
        let c = self.control(b.kind, p.sites, p.threads);
        med_max(
            p.seen
                .iter()
                .zip(&c.seen)
                .map(|(f, c)| growth(f).peak_rss - growth(c).peak_rss)
                .collect(),
        )
    }
}

/// The share of one site on one thread at `p`, in bytes.
fn per_site_thread(share: i64, p: &SitePoint) -> i64 {
    share / i64::try_from(p.sites * p.threads as usize).expect("a small count")
}

fn site_spec(glob: &str, work: &Path, dsl: String, threads: u32) -> Spec {
    Spec {
        glob: glob.to_owned(),
        dsl,
        threads,
        queries: 1,
        memory_limit: DEFAULT_QUERY_MEMORY_LIMIT,
        rss_cap: rss_cap(),
        work: work.to_owned(),
    }
}

/// Every run of `dsl` as `sites` sites on `threads` threads, after
/// checking that it passes `parse` and that `DuckDB` binds `sites` regex
/// calls for it.
fn site_runs(glob: &str, work: &Path, dsl: &str, sites: usize, threads: u32) -> Vec<Seen> {
    assert!(parse(dsl).is_ok(), "every measured query passes parse");
    assert_eq!(
        bound_sites(dsl, glob, true),
        u64::try_from(sites).expect("a small count"),
        "{dsl}"
    );
    let spec = site_spec(glob, work, dsl.to_owned(), threads);
    (0..REPS).map(|_| in_child(&spec)).collect()
}

/// The `site_size` part's table: one row a point.
fn site_table(results: &SiteResults, out: &mut String) {
    let _ = writeln!(
        out,
        "{:<42} {:>3} {:>5} {:>8} {:>5} {:>7} {:>17} {:>8} {:>17} {:>9} {:>9}  outcome",
        "body",
        "k",
        "bytes",
        "view KiB",
        "sites",
        "threads",
        "peak RSS",
        "control",
        "RE2 share",
        "per site",
        "after RSS"
    );
    for b in &results.bodies {
        for p in &b.points {
            let c = results.control(b.kind, p.sites, p.threads);
            let peak = med_max(p.seen.iter().map(|s| growth(s).peak_rss).collect());
            let ctl = med_max(c.seen.iter().map(|s| growth(s).peak_rss).collect());
            let share = results.share(b, p);
            let after = med_max(p.seen.iter().map(|s| growth(s).after_rss).collect());
            let mut outcome: Vec<String> = p.seen.iter().map(outcome_text).collect();
            if !all_ok(&c.seen) {
                let control: Vec<String> = c.seen.iter().map(outcome_text).collect();
                outcome.push(format!("control: {}", control.join("; ")));
            }
            let name: String = b.name.chars().take(42).collect();
            let _ = writeln!(
                out,
                "{name:<42} {:>3} {:>5} {:>8.1} {:>5} {:>7} {:>8.1} / {:>6.1} {:>8.1} {:>8.1} / {:>6.1} {:>9.2} {:>9.1}  {}",
                b.k,
                bound_text(b.kind, &b.body).len(),
                kib(b.view),
                p.sites,
                p.threads,
                mib(peak.0),
                mib(peak.1),
                mib(ctl.0),
                mib(share.0),
                mib(share.1),
                mib(per_site_thread(share.0, p)),
                mib(after.0),
                outcome.join("; ")
            );
        }
    }
}

/// Every run of the `site_size` part: the controls of each shape, then
/// each family's points.
fn measure_sites(
    glob: &str,
    work: &Path,
    control_k: usize,
    families: &[(SiteFamily, usize, String)],
) -> SiteResults {
    let mut controls = Vec::new();
    for kind in [Kind::Matches, Kind::Extract] {
        for &sites in &SITE_COUNTS {
            for &threads in &SITE_THREADS {
                let body = control(kind, control_k);
                let seen = site_runs(glob, work, &dsl(kind, &body, sites), sites, threads);
                controls.push(SiteControl {
                    kind,
                    sites,
                    threads,
                    seen,
                });
            }
        }
    }

    let mut bodies = Vec::new();
    for (f, k, body) in families {
        let mut points = Vec::new();
        for &sites in &SITE_COUNTS {
            for &threads in &SITE_THREADS {
                let seen = site_runs(glob, work, &dsl(f.kind, body, sites), sites, threads);
                points.push(SitePoint {
                    sites,
                    threads,
                    seen,
                });
            }
        }
        bodies.push(SiteBody {
            name: f.name.to_owned(),
            kind: f.kind,
            ordinary: false,
            k: *k,
            body: body.clone(),
            view: view_bytes(f.kind, body),
            points,
        });
    }
    SiteResults {
        control_k,
        controls,
        bodies,
    }
}

/// The corpus's `c` rule: one more than the first `extract` family's
/// count, so each row ends in one match of it.
fn site_gap(families: &[SiteFamily]) -> usize {
    families
        .iter()
        .find(|f| f.kind == Kind::Extract)
        .and_then(site_body)
        .map_or(2, |(k, _)| k + 1)
}

/// The controls' count: the span the corpus's rows end in, or less where
/// 16 copies of a control would not pass `parse` at it (a `matches` copy's
/// suffix holds three positions more).
fn site_control_k(gap: usize) -> usize {
    let n = SITE_COUNTS[SITE_COUNTS.len() - 1];
    largest(gap - 1, |k| {
        k == 0
            || [Kind::Matches, Kind::Extract]
                .iter()
                .all(|&kind| parse(&dsl(kind, &control(kind, k), n)).is_ok())
    })
    .unwrap_or(1)
}

/// The `site_size` part: what one site of RE2 holds for single bodies at
/// the full body limit, as 1, 4 and 16 copies on 1 and 4 threads, each
/// beside a control of the same shape.
#[test]
#[ignore = "resident-memory evidence for a reader; run through scripts/evidence/issue-335/re2.sh"]
fn site_size() {
    let started = Instant::now();
    let mut out = String::new();
    header("site_size", &mut out);
    let work = work_dir();
    let families: Vec<(SiteFamily, usize, String)> = site_families()
        .into_iter()
        .map(|f| {
            let (k, body) = site_body(&f).unwrap_or_else(|| panic!("{} has an edge", f.name));
            (f, k, body)
        })
        .collect();
    // One corpus for every body, with the `c` rule at the forward extract
    // family's span, so each of its rows holds one match, at the end.
    let gap = site_gap(&site_families());
    let control_k = site_control_k(gap);
    let (glob, chars) = write_corpus(
        &work.join("corpus-site"),
        SWEEP_ROWS_PER_FILE,
        gap,
        Kind::Extract,
    );
    let _ = writeln!(
        out,
        "single bodies at the full body limit, as 1, 4 and 16 copies in one query on 1 and {DEFAULT_QUERY_THREADS} database threads"
    );
    let _ = writeln!(
        out,
        "corpus: {FILES} parquet files of {SWEEP_ROWS_PER_FILE} rows, {chars} characters in all, seven rows in eight Deseret, the rest CJK, ASCII and Specials in turn; every row ends in a, {} b's and c",
        gap - 1
    );
    let _ = writeln!(
        out,
        "C is [abc], N is [^abc] and FAN32 the fan class, over Deseret; a padded body is (?:core|q...) with the most q's parse admits; view is the compiled size of the batch view of the text one site binds, the measure the 512 KiB cap uses"
    );
    let _ = writeln!(
        out,
        "every query passes trawl_core::parse, and one regexp_ call runs for each site (counted off DuckDB's optimized plan); the controls are the same shapes of a b{{{control_k}}} N (matches) and a b{{{control_k}}} c (extract), whose DFAs have at most {} states",
        control_k + 2
    );
    let _ = writeln!(
        out,
        "growth over the baseline in MiB, median / max of {REPS} runs, each in a fresh process; RE2's share is each run over the control run beside it; per site is the median share over sites x threads\n"
    );

    let results = measure_sites(&glob, &work, control_k, &families);
    site_table(&results, &mut out);
    std::fs::write(
        work.join(SITE_RESULTS),
        serde_json::to_string(&results).expect("the results"),
    )
    .expect("write the results");
    footer("site_size", started, &mut out);
    print!("{out}");
}

/// One measured point, for the fit: a body's view, its kind, and RE2's
/// share of one site on one thread, the median and the largest run.
#[derive(Clone, Copy, Debug)]
struct Fit {
    view: usize,
    kind: Kind,
    ordinary: bool,
    median: i64,
    max: i64,
}

/// `bytes` raised by [`MARGIN_PERCENT`] and rounded up to 64 KiB.
fn with_margin(bytes: i64) -> u64 {
    let bytes = u64::try_from(bytes.max(0)).expect("non-negative");
    let raised = (bytes * (100 + MARGIN_PERCENT)).div_ceil(100);
    raised.div_ceil(64 << 10) * (64 << 10)
}

/// An allowance model: per site per thread, `a + b × view` bytes, with
/// `b` in parts per million.
#[derive(Clone, Copy, Debug)]
struct Model {
    a: u64,
    b_ppm: u64,
}

impl Model {
    fn charge(self, view: usize) -> u64 {
        let view = u64::try_from(view).expect("a view size");
        self.a + (self.b_ppm * view).div_ceil(1_000_000)
    }
}

/// The size model: a least-squares slope of the per-site figure over the
/// view across the `kinds` points, clamped at zero, and the intercept
/// that puts the line at or above every point (median and max), both
/// raised by the margin.
#[expect(clippy::cast_precision_loss, reason = "a fit over byte counts")]
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a clamped, non-negative slope in parts per million"
)]
fn size_model(fits: &[Fit]) -> Model {
    let n = fits.len() as f64;
    let mx = fits.iter().map(|f| f.view as f64).sum::<f64>() / n;
    let my = fits.iter().map(|f| f.median as f64).sum::<f64>() / n;
    let sxy: f64 = fits
        .iter()
        .map(|f| (f.view as f64 - mx) * (f.median as f64 - my))
        .sum();
    let sxx: f64 = fits.iter().map(|f| (f.view as f64 - mx).powi(2)).sum();
    let slope = if sxx > 0.0 { (sxy / sxx).max(0.0) } else { 0.0 };
    let b_ppm = (slope * 1e6).ceil() as u64;
    let a = fits
        .iter()
        .map(|f| {
            f.max.max(f.median)
                - i64::try_from(Model { a: 0, b_ppm }.charge(f.view)).expect("a charge")
        })
        .max()
        .unwrap_or(0);
    Model {
        a: with_margin(a),
        b_ppm: b_ppm * (100 + MARGIN_PERCENT) / 100,
    }
}

/// The flat model: the worst point (median or max), raised by the margin.
fn flat_model(fits: &[Fit]) -> Model {
    let worst = fits.iter().map(|f| f.max.max(f.median)).max().unwrap_or(0);
    Model {
        a: with_margin(worst),
        b_ppm: 0,
    }
}

/// The access-log parser: the corpus's one `extract`-shaped body, with its
/// named groups.
fn access_log() -> &'static str {
    trawl_core::pattern::charge::LOG_REGEXES
        .iter()
        .copied()
        .find(|b| b.starts_with("^(?P<ip>"))
        .expect("the corpus holds the access-log parser")
}

/// The ordinary query of the stop rule: the corpus's first five patterns
/// as one search.
fn ordinary_query() -> (String, Vec<&'static str>) {
    let five: Vec<&str> = trawl_core::pattern::charge::LOG_REGEXES
        .iter()
        .copied()
        .take(5)
        .collect();
    let list: Vec<String> = five.iter().map(|b| format!("/{b}/")).collect();
    (format!("message={}", list.join(",")), five)
}

/// What each model (name, its `regexp_matches` charge, its
/// `regexp_extract` charge) charges the ordinary queries on the default
/// threads, against the stop rule.
fn ordinary_charges(work: &Path, models: [(&str, Model, Model); 3], out: &mut String) {
    let (query, five) = ordinary_query();
    let glob = format!("{}/*.parquet", work.join("corpus-site").display());
    let log = format!("* | extract \"{}\"", access_log());
    let (log_sites, log_bound) = (
        bound_sites(&log, &glob, true),
        bound_sites(&log, &glob, false),
    );
    let five_sites = bound_sites(&query, &glob, true);
    let threads = u64::from(DEFAULT_QUERY_THREADS);
    let five_views: Vec<usize> = five
        .iter()
        .map(|b| {
            trawl_core::pattern::Preparer::new()
                .batch_compiled_bytes(b)
                .expect("measures")
        })
        .collect();
    let log_view = trawl_core::pattern::Preparer::new()
        .batch_compiled_bytes(access_log())
        .expect("measures");
    let _ = writeln!(
        out,
        "\nordinary queries, on {threads} threads (query_threads' default):"
    );
    let _ = writeln!(
        out,
        "  five corpus patterns, {query}: DuckDB binds {five_sites} regexp_ calls; views {} bytes",
        five_views
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );
    let _ = writeln!(
        out,
        "  the access-log parser, {log}: {log_sites} regexp_ calls run (the optimized plan) and DuckDB binds {log_bound} (the unoptimized plan: nullif binds each call twice, and the optimizer folds the copies); view {log_view} bytes; its charge below counts the {log_sites} that run, and twice that if the reservation counts bound copies"
    );
    let _ = writeln!(
        out,
        "\n{:<8} {:>14} {:>14} {:>22}  stop rule: {STOP_QUERIES} ordinary queries within {} MiB",
        "model",
        "five patterns",
        "access log",
        "ordinary queries / GiB",
        STOP_RESERVATION / MIB
    );
    for (name, m, log_model) in models {
        let five_charge: u64 = five_views.iter().map(|&v| m.charge(v)).sum::<u64>() * threads;
        let log_charge = log_model.charge(log_view) * log_sites * threads;
        let fits_in = STOP_RESERVATION / five_charge.max(1);
        let _ = writeln!(
            out,
            "{name:<8} {:>10.1} MiB {:>10.1} MiB {fits_in:>22}  {}",
            mib(signed(five_charge)),
            mib(signed(log_charge)),
            if fits_in >= STOP_QUERIES {
                "met"
            } else {
                "NOT met"
            }
        );
    }
    let _ = writeln!(
        out,
        "(the by-kind model charges a regexp_matches site at the matches figure and a regexp_extract site at the extract figure; the five patterns run as regexp_matches, the access-log parser as regexp_extract)"
    );
}

/// Every body under each model (flat, size, and by kind: the matches
/// and the extract figure), and the smallest charge over a body's worst
/// point.
#[expect(clippy::cast_precision_loss, reason = "ratios for a reader")]
fn model_table(results: &SiteResults, models: [Model; 4], out: &mut String) {
    let [flat, sized, matches_flat, extract_flat] = models;
    let _ = writeln!(
        out,
        "\n{:<42} {:>7} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "body", "kind", "view KiB", "per site", "flat", "size", "by kind"
    );
    let mut margins = (f64::MAX, f64::MAX, f64::MAX);
    for b in &results.bodies {
        let by_kind = match b.kind {
            Kind::Matches => matches_flat,
            Kind::Extract => extract_flat,
        };
        let worst_point = b
            .points
            .iter()
            .map(|p| {
                let (m, x) = results.share(b, p);
                per_site_thread(m, p).max(per_site_thread(x, p))
            })
            .max()
            .unwrap_or(0);
        let ratio = |m: Model| m.charge(b.view) as f64 / worst_point.max(1) as f64;
        margins.0 = margins.0.min(ratio(flat));
        margins.1 = margins.1.min(ratio(sized));
        margins.2 = margins.2.min(ratio(by_kind));
        let name: String = b.name.chars().take(42).collect();
        let _ = writeln!(
            out,
            "{name:<42} {:>7} {:>9.1} {:>9.2} {:>9.2} {:>9.2} {:>9.2}",
            format!("{:?}", b.kind).to_lowercase(),
            kib(b.view),
            mib(worst_point),
            mib(signed(flat.charge(b.view))),
            mib(signed(sized.charge(b.view))),
            mib(signed(by_kind.charge(b.view)))
        );
    }
    let _ = writeln!(
        out,
        "(per site: the worst of the body's points, median or single run; the models' charges for one site on one thread)"
    );
    let _ = writeln!(
        out,
        "smallest charge over a body's worst point: flat {:.2}x, size {:.2}x, by kind {:.2}x",
        margins.0, margins.1, margins.2
    );
}

/// Every measured point of the `site_size` results, per site per thread.
fn site_fits(results: &SiteResults) -> Vec<Fit> {
    results
        .bodies
        .iter()
        .flat_map(|b| {
            b.points.iter().map(move |p| {
                let (m, x) = results.share(b, p);
                Fit {
                    view: b.view,
                    kind: b.kind,
                    ordinary: b.ordinary,
                    median: per_site_thread(m, p),
                    max: per_site_thread(x, p),
                }
            })
        })
        .collect()
}

/// Every model is at or above every point it covers: the flat and size
/// models every point, each by-kind figure its own kind's points.
fn assert_covered(fits: &[Fit], models: [Model; 4]) {
    let [flat, sized, matches_flat, extract_flat] = models;
    let covers = |m: Model, f: &Fit| {
        m.charge(f.view) >= u64::try_from(f.max.max(f.median).max(0)).expect("a share")
    };
    for f in fits {
        let by_kind = match f.kind {
            Kind::Matches => matches_flat,
            Kind::Extract => extract_flat,
        };
        assert!(
            covers(flat, f) && covers(sized, f) && covers(by_kind, f),
            "every model is at or above every point it covers: {f:?}"
        );
    }
}

/// The ordinary bodies, each as [`ORDINARY_SITES`] copies on the default
/// threads, beside the `site_size` part's control of that shape: their
/// rows of the table, and their points added to `results`.
fn measure_ordinary(results: &mut SiteResults, work: &Path, out: &mut String) {
    let glob = format!("{}/*.parquet", work.join("corpus-site").display());
    let (glob, work) = (glob.as_str(), work);
    let mut bodies = Vec::new();
    for body in ordinary_bodies() {
        let seen = site_runs(
            glob,
            work,
            &dsl(Kind::Matches, &body, ORDINARY_SITES),
            ORDINARY_SITES,
            DEFAULT_QUERY_THREADS,
        );
        bodies.push(SiteBody {
            name: body.clone(),
            kind: Kind::Matches,
            ordinary: true,
            k: 0,
            view: view_bytes(Kind::Matches, &body),
            body,
            points: vec![SitePoint {
                sites: ORDINARY_SITES,
                threads: DEFAULT_QUERY_THREADS,
                seen,
            }],
        });
    }
    let _ = writeln!(
        out,
        "the ordinary bodies (the log-pattern corpus and error|warn), each as {ORDINARY_SITES} copies on {DEFAULT_QUERY_THREADS} threads over the site_size part's corpus, beside its control of that shape; growth in MiB, median / max of {REPS} runs\n"
    );
    let ordinary = SiteResults {
        control_k: results.control_k,
        controls: results.controls.clone(),
        bodies,
    };
    site_table(&ordinary, out);
    results.bodies.extend(ordinary.bodies);
    let _ = writeln!(out);
}

/// The `allowance` part: fit the allowance per site per thread to the
/// `site_size` results, and state what it charges ordinary queries.
#[test]
#[ignore = "resident-memory evidence for a reader; run through scripts/evidence/issue-335/re2.sh"]
#[expect(clippy::cast_precision_loss, reason = "MiB figures for a reader")]
fn allowance() {
    let work = work_dir();
    let text = std::fs::read_to_string(work.join(SITE_RESULTS))
        .expect("the site_size part's results; run it first");
    let mut results: SiteResults = serde_json::from_str(&text).expect("the results");
    let mut out = String::new();
    header("allowance", &mut out);
    measure_ordinary(&mut results, &work, &mut out);

    let fits = site_fits(&results);
    let worst = |pick: &dyn Fn(&Fit) -> bool| {
        fits.iter()
            .filter(|f| pick(f))
            .map(|f| f.median)
            .max()
            .unwrap_or(0)
    };
    let worst_max = fits.iter().map(|f| f.max).max().unwrap_or(0);
    let _ = writeln!(
        out,
        "per site is RE2's share of one site on one thread: the share over sites x threads. Worst median per site: {:.2} MiB over every body, {:.2} MiB over the matches bodies, {:.2} MiB over the extract bodies, {:.2} MiB over the ordinary bodies; worst single run {:.2} MiB.",
        mib(worst(&|_| true)),
        mib(worst(&|f| f.kind == Kind::Matches && !f.ordinary)),
        mib(worst(&|f| f.kind == Kind::Extract)),
        mib(worst(&|f| f.ordinary)),
        mib(worst_max)
    );

    let heavy: Vec<Fit> = fits.iter().copied().filter(|f| !f.ordinary).collect();
    let views: Vec<usize> = heavy.iter().map(|f| f.view).collect();
    let flat = flat_model(&fits);
    let sized = size_model(&fits);
    let matches_flat = flat_model(
        &fits
            .iter()
            .copied()
            .filter(|f| f.kind == Kind::Matches)
            .collect::<Vec<_>>(),
    );
    let extract_flat = flat_model(
        &fits
            .iter()
            .copied()
            .filter(|f| f.kind == Kind::Extract)
            .collect::<Vec<_>>(),
    );
    let _ = writeln!(
        out,
        "views of the measured bodies: {:.1} to {:.1} KiB",
        kib(*views.iter().min().unwrap_or(&0)),
        kib(*views.iter().max().unwrap_or(&0))
    );
    let _ = writeln!(
        out,
        "models, per site per thread, each at least {MARGIN_PERCENT} % over the worst point it covers (median or single run), rounded up to 64 KiB:"
    );
    let _ = writeln!(
        out,
        "  flat:          {:.2} MiB a site, whatever the body",
        mib(signed(flat.a))
    );
    let _ = writeln!(
        out,
        "  size:          {:.2} MiB + {:.3} x view bytes (least-squares slope over every point, clamped at 0, intercept at or above every point)",
        mib(signed(sized.a)),
        sized.b_ppm as f64 / 1e6
    );
    let _ = writeln!(
        out,
        "  flat by kind:  {:.2} MiB a regexp_matches site, {:.2} MiB a regexp_extract site",
        mib(signed(matches_flat.a)),
        mib(signed(extract_flat.a))
    );

    model_table(
        &results,
        [flat, sized, matches_flat, extract_flat],
        &mut out,
    );
    assert_covered(&fits, [flat, sized, matches_flat, extract_flat]);
    ordinary_charges(
        &work,
        [
            ("flat", flat, flat),
            ("size", sized, sized),
            ("by kind", matches_flat, extract_flat),
        ],
        &mut out,
    );
    print!("{out}");
}

#[test]
fn every_site_family_reaches_the_body_limit() {
    for f in site_families() {
        let (k, body) = site_body(&f).unwrap_or_else(|| panic!("{} has an edge", f.name));
        for n in SITE_COUNTS {
            assert!(parse(&dsl(f.kind, &body, n)).is_ok(), "{} x {n}", f.name);
        }
        if f.pad {
            let over = padded(&(f.core)(k), body.chars().filter(|&c| c == PAD).count() + 1);
            assert!(
                parse(&dsl(f.kind, &over, SITE_COUNTS[2])).is_err(),
                "{}: one more byte is refused",
                f.name
            );
        }
    }
    for body in ordinary_bodies() {
        assert!(
            parse(&dsl(Kind::Matches, &body, ORDINARY_SITES)).is_ok(),
            "{body}"
        );
    }
    let control_k = site_control_k(site_gap(&site_families()));
    for kind in [Kind::Matches, Kind::Extract] {
        for n in SITE_COUNTS {
            let q = dsl(kind, &control(kind, control_k), n);
            assert!(parse(&q).is_ok(), "{q}: {:?}", parse(&q).err());
        }
    }
}

/// `DuckDB` binds one `regexp_` call for each site the `site_size` part
/// counts, and the ordinary queries bind what the `allowance` part says.
#[test]
fn duckdb_binds_one_regex_call_a_site() {
    let dir = tempfile::tempdir().expect("a corpus dir");
    let (glob, _) = write_corpus(dir.path(), 1, 2, Kind::Extract);
    for f in site_families() {
        let (_, body) = site_body(&f).expect("an edge");
        for n in SITE_COUNTS {
            let q = dsl(f.kind, &body, n);
            assert_eq!(bound_sites(&q, &glob, true), n as u64, "{}", f.name);
        }
    }
    let (five, _) = ordinary_query();
    assert_eq!(bound_sites(&five, &glob, true), 5);
    assert_eq!(bound_sites(&five, &glob, false), 5);
    let log = format!("* | extract \"{}\"", access_log());
    assert_eq!(bound_sites(&log, &glob, true), 7, "one call a named group");
    assert_eq!(
        bound_sites(&log, &glob, false),
        14,
        "nullif binds each twice"
    );
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
