// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! How long a timed-out batch regex query keeps its executor permit
//! (issue #334, ADR-0063).
//!
//! `DuckDB` checks for an interrupt between the vectors of rows it filters,
//! never inside the `regexp_matches` calls over one vector. A query that
//! times out therefore answers at its deadline, but its `DuckDB` work, and
//! with it the executor permit, runs on until each thread finishes the
//! vector it holds. This probe measures that overhang through
//! trawl-server's own `ExecutorPool::execute`, the `/query` lane, built by
//! `ExecutorPool::for_test` under the server's default budgets (4 `DuckDB`
//! threads), with one permit and no hot buffer. It records three instants:
//!
//! - the request answers: `execute` returns, a timeout at the deadline;
//! - the `DuckDB` work stops: the worker reaches `Seam::Finished`;
//! - the permit is free again: the worker reaches `Seam::Released` and the
//!   pool shows the permit available. A monitor thread polls every 20 ms.
//!
//! The corpus is generated, never committed: cold parquet written by
//! `DuckDB`'s `COPY … (FORMAT PARQUET)` with its defaults, as the hot buffer
//! flushes, laid out as `prod/2026-11-03/{HH}/cjk.parquet`, one file per
//! hour. Each `message` is distinct (a xorshift stream seeded per row),
//! eight tenths 漢 and the rest 字 and 文, so no dictionary entry is shared
//! and no row matches. A corpus directory with a `DONE-…` stamp of the same
//! shape is reused.
//!
//! The bodies, each admitted by `trawl_core::parser::parse` before the run:
//!
//! - `plain`: `漢[漢字文]{62}[^漢字文]`, 64 positions;
//! - `fan`: `漢F{48}[^漢字文]`, where `F` is the fan class of the cost
//!   harness (`crates/trawl-engine/tests/regex_cost.rs`) over the same
//!   three ideographs.
//!
//! Usage:
//!
//! ```text
//! issue-334-cancel-probe <corpus_dir> <plain|fan> <chars_per_message>
//!     <rows_per_file> <files> <deadline_secs> <observe_secs>
//! ```
//!
//! Observation stops `observe_secs` after the start, and whatever has not
//! happened by then is reported as "not by". The process then exits, which
//! ends any work still running.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use trawl_server::deadline::Deadline;
use trawl_server::pool::seam::Seam;
use trawl_server::pool::{ExecutorPool, WorkContext, WorkKind};

const A: char = '\u{6F22}'; // 漢
const B: char = '\u{5B57}'; // 字
const C: char = '\u{6587}'; // 文

fn loadavg() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .map(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
        .unwrap_or_default()
}

/// The fan class over `A`, `B` and `C`: the cost harness's `fan` for its
/// CJK alphabet. Alternating ASCII letters and digits, one character for
/// each UTF-8 lead byte, and along `A`'s UTF-8 path one character for each
/// sibling continuation byte, every other one at the last level, so every
/// list of byte ranges the subject's bytes walk through is long.
fn fan() -> String {
    let ch = |bytes: &[u32]| {
        let bytes: Vec<u8> = bytes
            .iter()
            .map(|&b| u8::try_from(b).expect("a byte"))
            .collect();
        std::str::from_utf8(&bytes)
            .expect("one UTF-8 character")
            .chars()
            .next()
            .expect("one character")
    };
    let mut chars: BTreeSet<char> = "ACEGIKMOQSUWYdfhjlnprtvxz02468".chars().collect();
    for lead in 0xC2..0xE0 {
        chars.insert(ch(&[lead, 0x80 + (lead * 5) % 64]));
    }
    for lead in (0xE1..0xED).chain([0xEE, 0xEF]) {
        chars.insert(ch(&[lead, 0x80 + (lead * 3) % 64, 0x80 + (lead * 7) % 64]));
    }
    for lead in 0xF0..0xF4 {
        let second = if lead == 0xF0 { 0x90 } else { 0x80 };
        chars.insert(ch(&[lead, second, 0x81, 0x81]));
    }
    let mut buf = [0; 4];
    let path: Vec<u32> = A.encode_utf8(&mut buf).bytes().map(u32::from).collect();
    for b2 in (0x80..0xC0).filter(|&b| b != path[1]) {
        chars.insert(ch(&[path[0], b2, 0x80 + (b2 * 7) % 64]));
    }
    for b3 in (0x80..0xC0).step_by(2) {
        chars.insert(ch(&[path[0], path[1], b3]));
    }
    chars.extend([A, B, C]);
    let inner: String = chars
        .into_iter()
        .map(|c| {
            if c.is_ascii_punctuation() {
                format!("\\{c}")
            } else {
                c.to_string()
            }
        })
        .collect();
    format!("[{inner}]")
}

/// The body a case names.
fn body(case: &str) -> String {
    match case {
        "plain" => format!("{A}[{A}{B}{C}]{{62}}[^{A}{B}{C}]"),
        "fan" => format!("{A}{}{{48}}[^{A}{B}{C}]", fan()),
        other => panic!("unknown case {other}: want plain or fan"),
    }
}

/// `body` as the transcript prints it: a control character, which the fan
/// class holds one of, is written as its escape.
fn printable(body: &str) -> String {
    body.chars()
        .map(|c| {
            if c.is_control() {
                format!("\\x{{{:X}}}", u32::from(c))
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// Row `row` of the corpus: `chars` characters, eight tenths `A`, the
/// rest `B` and `C`, from a xorshift stream seeded by the row, so every
/// row differs (no dictionary entry is shared).
fn message(row: u64, chars: usize) -> String {
    let mut s = 0x9E37_79B9_7F4A_7C15_u64 ^ (row.wrapping_mul(0xA24B_AED4_963E_E407) | 1);
    let mut out = String::with_capacity(chars * 3);
    for _ in 0..chars {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        out.push(match s % 10 {
            0..8 => A,
            8 => B,
            _ => C,
        });
    }
    out
}

/// Write the corpus under `dir`, unless a stamp of the same shape says it
/// is there. Reports on stdout name paths relative to `dir` only.
fn write_corpus(dir: &Path, chars: usize, rows: usize, files: usize) {
    let stamp = dir.join(format!("DONE-{chars}-{rows}-{files}"));
    if stamp.exists() {
        println!("corpus: reused, {files} files of {rows} rows of {chars} characters");
        return;
    }
    let _ = std::fs::remove_dir_all(dir);
    let started = Instant::now();
    for f in 0..files {
        let relative = format!("prod/2026-11-03/{f:02}/cjk.parquet");
        let path = dir.join(&relative);
        std::fs::create_dir_all(path.parent().expect("an hour dir")).expect("hour dir");
        let conn = duckdb::Connection::open_in_memory().expect("duckdb");
        conn.execute_batch(
            "CREATE TABLE t (_time TIMESTAMP, _ingested TIMESTAMP, _raw VARCHAR, \
             _repairs VARCHAR, env VARCHAR, service VARCHAR, host VARCHAR, \
             severity BIGINT, severity_text VARCHAR, message VARCHAR)",
        )
        .expect("create");
        {
            let mut app = conn.appender("t").expect("appender");
            for r in 0..rows {
                let row = u64::try_from(f * rows + r).expect("a row number");
                let time = format!("2026-11-03 {f:02}:00:{:02}.{:06}", r % 60, r);
                app.append_row(duckdb::params![
                    time,
                    time,
                    format!("event {row}"),
                    Option::<String>::None,
                    "prod",
                    "cjk",
                    "h1",
                    9_i64,
                    "info",
                    message(row, chars),
                ])
                .expect("append");
            }
        }
        conn.execute_batch(&format!("COPY t TO '{}' (FORMAT PARQUET)", path.display()))
            .expect("copy");
        let (groups, sizes): (i64, String) = conn
            .query_row(
                &format!(
                    "SELECT count(DISTINCT row_group_id), \
                     string_agg(DISTINCT row_group_num_rows::VARCHAR, ',') \
                     FROM parquet_metadata('{}')",
                    path.display()
                ),
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("metadata");
        println!(
            "corpus: wrote {relative} ({rows} rows, {groups} row groups of {sizes} rows, {} bytes)",
            std::fs::metadata(&path).map_or(0, |m| m.len())
        );
    }
    std::fs::write(&stamp, b"").expect("stamp");
    println!("corpus: built in {:.1} s", started.elapsed().as_secs_f64());
}

/// The body must be admitted at this tree, or the probe proves nothing.
fn check_admitted(dsl: &str) {
    match trawl_core::parser::parse(dsl) {
        // The transcript names the crate with a hyphen: the evidence check
        // reads `trawl_` and a letter as the start of an API key.
        Ok(_) => println!("admission: admitted by trawl-core's parser::parse"),
        Err(e) => panic!("the body is refused: {:?}", e.first().map(|e| &e.message)),
    }
}

#[expect(
    clippy::cast_precision_loss,
    reason = "millisecond instants printed as seconds"
)]
fn secs(ms: u64) -> f64 {
    ms as f64 / 1000.0
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
#[expect(clippy::too_many_lines, reason = "one probe run, start to finish")]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [
        _,
        dir,
        case,
        chars,
        rows,
        files,
        deadline_secs,
        observe_secs,
    ] = &args[..]
    else {
        panic!(
            "usage: issue-334-cancel-probe <corpus_dir> <plain|fan> <chars> <rows_per_file> <files> <deadline_secs> <observe_secs>"
        );
    };
    let chars: usize = chars.parse().expect("chars");
    let rows: usize = rows.parse().expect("rows");
    let files: usize = files.parse().expect("files");
    let deadline_secs: u64 = deadline_secs.parse().expect("deadline");
    let observe_secs: u64 = observe_secs.parse().expect("observe");
    let body = body(case);
    let dir = Path::new(dir);
    write_corpus(dir, chars, rows, files);

    let dsl = format!("message=/{body}/");
    check_admitted(&dsl);

    let pool = ExecutorPool::for_test(dir.display().to_string(), 1, 1000, None).expect("pool");
    let seams = pool.seams();
    let finished = seams.watch(Seam::Finished);
    let released = seams.watch(Seam::Released);
    let started = seams.watch(Seam::Started);

    println!(
        "case {case}: {chars}-character CJK messages, {rows} rows a file, {files} files, deadline {deadline_secs} s, observed up to {observe_secs} s"
    );
    println!("body ({} bytes): {}", body.len(), printable(&body));
    println!("load average at start: {}", loadavg());

    let t0 = Instant::now();
    let deadline = Deadline::after(Duration::from_secs(deadline_secs));
    let never = u64::MAX;
    let t_started = Arc::new(AtomicU64::new(never));
    let t_finished = Arc::new(AtomicU64::new(never));
    let t_released = Arc::new(AtomicU64::new(never));
    let t_permit = Arc::new(AtomicU64::new(never));
    let monitor = {
        let pool = pool.clone();
        let (t_started, t_finished, t_released, t_permit) = (
            Arc::clone(&t_started),
            Arc::clone(&t_finished),
            Arc::clone(&t_released),
            Arc::clone(&t_permit),
        );
        std::thread::spawn(move || {
            let mut last_note = 0_u64;
            loop {
                let ms = u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX);
                let mark = |a: &AtomicU64, hit: bool| {
                    if hit && a.load(Ordering::SeqCst) == never {
                        a.store(ms, Ordering::SeqCst);
                    }
                };
                mark(&t_started, started.arrivals() > 0);
                mark(&t_finished, finished.arrivals() > 0);
                mark(&t_released, released.arrivals() > 0);
                mark(
                    &t_permit,
                    t_started.load(Ordering::SeqCst) != never && pool.available_permits() == 1,
                );
                if t_permit.load(Ordering::SeqCst) != never || ms > observe_secs * 1000 {
                    break;
                }
                if ms / 30_000 > last_note {
                    last_note = ms / 30_000;
                    println!(
                        "  t = {:>6.1} s: permit held: {}, retained: {}, load average {}",
                        secs(ms),
                        pool.available_permits() == 0,
                        pool.retained(),
                        loadavg()
                    );
                    let _ = std::io::stdout().flush();
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };

    let outcome = pool
        .execute(
            pool.allocate_query_id(),
            &dsl,
            deadline,
            false,
            0,
            WorkContext::system(WorkKind::Query),
        )
        .await;
    let answered = t0.elapsed();
    let answer = match &outcome.result {
        Ok(r) => format!("ok, {} rows", r.rows.len()),
        Err(e) => format!("error: {e}"),
    };
    println!(
        "request answered at {:.3} s: {answer}; load average {}",
        answered.as_secs_f64(),
        loadavg()
    );
    let _ = std::io::stdout().flush();
    monitor.join().expect("monitor");

    let show = |a: &AtomicU64| {
        let v = a.load(Ordering::SeqCst);
        if v == never {
            format!("not by {observe_secs} s")
        } else {
            format!("{:.3} s", secs(v))
        }
    };
    println!("work started:              {}", show(&t_started));
    println!("deadline:                  {deadline_secs}.000 s");
    println!("request answered:          {:.3} s", answered.as_secs_f64());
    println!(
        "DuckDB work stopped:       {} (Seam::Finished)",
        show(&t_finished)
    );
    println!(
        "slot released:             {} (Seam::Released)",
        show(&t_released)
    );
    println!("permit free again:         {}", show(&t_permit));
    let permit = t_permit.load(Ordering::SeqCst);
    if permit == never {
        println!(
            "permit outlived the deadline by more than {} s; retained now: {}",
            observe_secs - deadline_secs,
            pool.retained()
        );
    } else {
        println!(
            "permit outlived the deadline by {:.3} s",
            secs(permit) - secs(deadline_secs * 1000)
        );
    }
    println!("load average at end: {}", loadavg());
    let _ = std::io::stdout().flush();
    // The work may still run; ending the process ends it.
    std::process::exit(0);
}
