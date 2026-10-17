// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The query database's budgets against the bundled `DuckDB` (ADR-0056):
//! they read back from a clone, and a spill past the cap fails as
//! [`EngineError::ResourceExhausted`] without the spill directory ever
//! holding more than the cap plus one block of each temp-file size class.

use std::num::{NonZeroU32, NonZeroU64};
use std::path::{Path, PathBuf};
use std::time::Duration;

use trawl_engine::error::EngineError;
use trawl_engine::executor::{Executor, QueryBudgets};
use trawl_engine::scratch::{QueryScratch, is_spill_dir_name};

const MIB: u64 = 1024 * 1024;

/// `DuckDB` 1.5.5's spill block size (`DEFAULT_BLOCK_ALLOC_SIZE`).
const BLOCK: u64 = 262_144;

/// What the spill directory may hold past the cap on `DuckDB` 1.5.5: one
/// block of each temp-file size class, 32 KiB to 256 KiB in 32 KiB steps.
///
/// `DuckDB` accounts a temp file as its highest block index in use, but
/// truncates a shrinking file one block past that
/// (`TemporaryFileManager::RemoveTempBlockIndex`), and keeps one file per
/// size class until a class passes 4000 blocks, about 1000 MiB. So each
/// class's file can hold one block it does not count. The spill probe
/// measured up to 1.9 blocks past the cap.
const CLASS_SLACK: u64 = 1_179_648;

fn budgets(spill: u64, memory: u64, threads: u32) -> QueryBudgets {
    QueryBudgets {
        spill_max_bytes: NonZeroU64::new(spill).unwrap(),
        memory_limit: NonZeroU64::new(memory).unwrap(),
        threads: NonZeroU32::new(threads).unwrap(),
    }
}

/// The one spill directory the query database made in `scratch`.
fn spill_dir_in(scratch: &Path) -> PathBuf {
    let mut dirs: Vec<_> = std::fs::read_dir(scratch)
        .expect("read scratch")
        .map(|entry| entry.expect("entry").path())
        .collect();
    assert_eq!(dirs.len(), 1, "one spill directory: {dirs:?}");
    dirs.pop().unwrap()
}

#[test]
fn query_budgets_read_back_from_a_clone() {
    use std::os::unix::fs::MetadataExt as _;

    let scratch = tempfile::tempdir().expect("scratch");
    // Values off the 1000/1024 boundary, so a unit mix-up reads back wrong.
    let configured = budgets(48 * MIB, 96 * MIB, 3);
    let root = Executor::open_query_database(&QueryScratch::new(scratch.path()), &configured)
        .expect("the query database opens");

    let spill = spill_dir_in(scratch.path());
    assert!(
        is_spill_dir_name(spill.file_name().unwrap()),
        "{}",
        spill.display()
    );
    let meta = std::fs::symlink_metadata(&spill).expect("spill dir");
    assert!(meta.is_dir());
    assert_eq!(meta.mode() & 0o7777, 0o700);
    assert_eq!(meta.uid(), rustix::process::geteuid().as_raw());

    let clone = root.try_clone().expect("clone");
    let diagnostic = root.diagnostic_connection().expect("diagnostic");
    for (name, read) in [
        ("root", root.current_setting("temp_directory").unwrap()),
        ("clone", clone.current_setting("temp_directory").unwrap()),
        (
            "diagnostic",
            diagnostic.current_setting("temp_directory").unwrap(),
        ),
    ] {
        assert_eq!(Path::new(&read), spill, "{name}");
    }
    for exec in [&root, &clone] {
        assert_eq!(
            exec.current_setting("max_temp_directory_size").unwrap(),
            "48.0 MiB"
        );
        assert_eq!(exec.current_setting("memory_limit").unwrap(), "96.0 MiB");
        assert_eq!(exec.current_setting("threads").unwrap(), "3");
        assert_eq!(exec.current_setting("errors_as_json").unwrap(), "true");
    }
    assert_eq!(
        diagnostic.current_setting("memory_limit").unwrap(),
        "96.0 MiB",
        "the diagnostic connection shares the database's budgets"
    );
    assert_eq!(
        diagnostic.current_setting("errors_as_json").unwrap(),
        "true",
        "the diagnostic connection reports structured errors"
    );
    diagnostic
        .select_one()
        .expect("the diagnostic connection answers");

    drop((root, clone));
    assert!(spill.is_dir(), "the diagnostic connection still holds it");
    drop(diagnostic);
    assert!(!spill.exists(), "the last connection removed it");
}

/// Budgets `DuckDB` takes at a different value are refused at open, not
/// run under: a memory limit past what `DuckDB` will hold reads back as
/// something else.
#[test]
fn a_budget_that_reads_back_differently_refuses_the_open() {
    let scratch = tempfile::tempdir().expect("scratch");
    let error = Executor::open_query_database(
        &QueryScratch::new(scratch.path()),
        &budgets(32 * MIB, u64::MAX / 2, 2),
    )
    .expect_err("an unrepresentable memory limit");
    assert!(error.to_string().contains("memory_limit"), "{error}");
}

/// Write `rows` deterministic, poorly compressible 128-byte strings to a
/// Parquet file in `dir`: printable ASCII from a xorshift generator, so
/// zstd cannot shrink a spilled block by much. Row groups stay small: the
/// scan reads a column chunk whole, outside the spillable memory, and a
/// default 122,880-row group of these strings is a 16 MiB allocation.
fn random_strings_parquet(dir: &Path, rows: usize) -> PathBuf {
    let path = dir.join("strings.parquet");
    let conn = duckdb::Connection::open_in_memory().expect("fixture duckdb");
    conn.execute_batch("CREATE TABLE t (payload VARCHAR)")
        .expect("table");
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    {
        let mut appender = conn.appender("t").expect("appender");
        let mut text = String::with_capacity(128);
        for _ in 0..rows {
            text.clear();
            for _ in 0..16 {
                let mut word = next();
                for _ in 0..8 {
                    text.push(char::from(b' ' + u8::try_from(word % 95).unwrap()));
                    word /= 95;
                }
            }
            appender.append_row([text.as_str()]).expect("row");
        }
    }
    conn.execute_batch(&format!(
        "COPY t TO '{}' (FORMAT PARQUET, ROW_GROUP_SIZE 8192)",
        path.to_str().expect("utf-8")
    ))
    .expect("parquet");
    path
}

/// The largest byte totals the spill directory reached across samples:
/// the sum of the files' lengths and of their allocated blocks.
#[derive(Debug, Default, Clone, Copy)]
struct Peak {
    length: u64,
    allocated: u64,
    samples: u64,
}

/// The spill directory's totals right now.
fn totals(dir: &Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt as _;
    let (mut length, mut allocated) = (0, 0);
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                length += meta.len();
                allocated += meta.blocks() * 512;
            }
        }
    }
    (length, allocated)
}

/// Whether every thread of `pid` has stopped, or the process has exited
/// (a zombie never reads as stopped).
fn stopped_or_gone(pid: u32) -> bool {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return true;
    };
    tasks.flatten().all(|task| {
        std::fs::read_to_string(task.path().join("stat")).map_or(true, |stat| {
            // The state follows the parenthesised command name.
            stat.rsplit_once(") ")
                .is_none_or(|(_, rest)| rest.starts_with(['T', 't', 'Z', 'X']))
        })
    })
}

/// The child half of [`spill_under`]: run `TRAWL_SPILL_PROBE` in this
/// process and print its outcome: `ok <rows>`, or `err <class> <message>`
/// with the class [`EngineError`] gives the failure and `DuckDB`'s own
/// message. Ignored, and a no-op without the variable, so only the parent
/// runs it.
#[test]
#[ignore = "child process of the spill tests"]
fn spill_probe_child() {
    let Ok(spec) = std::env::var("TRAWL_SPILL_PROBE") else {
        return;
    };
    let mut parts = spec.splitn(5, '\t');
    let mut next = || parts.next().expect("probe spec field");
    let (spill, memory, threads, scratch, sql) = (
        next().parse().unwrap(),
        next().parse().unwrap(),
        next().parse().unwrap(),
        next().to_owned(),
        next().to_owned(),
    );
    let exec = Executor::open_query_database(
        &QueryScratch::new(&scratch),
        &budgets(spill, memory, threads),
    )
    .expect("open");
    let outcome = match exec.raw_count_rows_for_test(&sql) {
        Ok(rows) => format!("ok {rows}"),
        Err(error) => {
            // One line: the parent reads the outcome line by line.
            let message = error.to_string().replace('\n', " ");
            let class = match EngineError::from(error) {
                EngineError::ResourceExhausted => "resource_exhausted",
                EngineError::Database(_) => "database",
                _ => "other",
            };
            format!("err {class} {message}")
        }
    };
    println!("PROBE-OUTCOME {outcome}");
}

/// Run `sql` on a fresh query database under `budgets` in a child process
/// and sample its spill directory with the whole child stopped.
///
/// Each sample sends `SIGSTOP`, waits until every thread is stopped, reads
/// the directory, then sends `SIGCONT`. No thread moves a block while the
/// directory is read, so a sample is one consistent state of the files,
/// not a sum of states taken at different moments. Sampling can still
/// miss a peak between two samples.
fn spill_under(sql: &str, budgets: &QueryBudgets) -> (String, Peak) {
    use rustix::process::{Pid, Signal, kill_process};

    let scratch = tempfile::tempdir().expect("scratch");
    let spec = format!(
        "{}\t{}\t{}\t{}\t{sql}",
        budgets.spill_max_bytes,
        budgets.memory_limit,
        budgets.threads,
        scratch.path().to_str().expect("utf-8")
    );
    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", "spill_probe_child", "--ignored", "--nocapture"])
        .env("TRAWL_SPILL_PROBE", spec)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("probe child");
    let pid = Pid::from_child(&child);
    let mut peak = Peak::default();
    let mut spill = None;
    while child.try_wait().expect("child state").is_none() {
        if spill.is_none() {
            spill = std::fs::read_dir(scratch.path())
                .ok()
                .and_then(|mut entries| entries.next())
                .and_then(Result::ok)
                .map(|entry| entry.path());
        }
        let Some(dir) = &spill else {
            std::thread::yield_now();
            continue;
        };
        if kill_process(pid, Signal::STOP).is_err() {
            break;
        }
        while !stopped_or_gone(child.id()) {
            std::hint::spin_loop();
        }
        let (length, allocated) = totals(dir);
        let _ = kill_process(pid, Signal::CONT);
        peak.length = peak.length.max(length);
        peak.allocated = peak.allocated.max(allocated);
        peak.samples += 1;
        std::thread::sleep(Duration::from_micros(50));
    }
    let output = child.wait_with_output().expect("child output");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let outcome = stdout
        .lines()
        .find_map(|line| line.strip_prefix("PROBE-OUTCOME "))
        .unwrap_or("no outcome")
        .to_owned();
    (outcome, peak)
}

/// A full sort of every payload whose answer is one row: a window over the
/// whole input, so the sort cannot be a top-n and the result is not the
/// memory that runs out.
fn window_sort(source: &Path) -> String {
    format!(
        "SELECT max(rn) FROM (SELECT row_number() OVER (ORDER BY payload) AS rn \
         FROM read_parquet('{}'))",
        source.to_str().expect("utf-8")
    )
}

/// A hash aggregate over every distinct payload, answered with one row.
fn distinct_count(source: &Path) -> String {
    format!(
        "SELECT count(*) FROM (SELECT DISTINCT payload FROM read_parquet('{}'))",
        source.to_str().expect("utf-8")
    )
}

/// The spill probe (ADR-0056, AC4): the natural spill peak of each shape
/// under a cap it never reaches, then caps swept below that peak, where
/// the merge phase frees and rewrites blocks before the cap is hit.
///
/// Ignored: a measurement of the worst overshoot past the cap across both
/// shapes and two memory limits, kept for re-measuring
/// [`CLASS_SLACK`] when `DuckDB` is upgraded. Run it by hand with
/// `--run-ignored only --no-capture`. The bound itself is asserted by
/// [`spill_stays_within_budget_plus_class_slack`].
#[test]
#[ignore = "measurement, run by hand"]
fn spill_budget_probe() {
    let data = tempfile::tempdir().expect("data");
    let source = random_strings_parquet(data.path(), 1_000_000);
    for (shape, sql) in [
        ("window", window_sort(&source)),
        ("distinct", distinct_count(&source)),
    ] {
        for memory in [64 * MIB, 128 * MIB] {
            // `TRAWL_SPILL_PROBE_ONLY=window:64` runs one configuration.
            if let Ok(only) = std::env::var("TRAWL_SPILL_PROBE_ONLY")
                && only != format!("{shape}:{}", memory / MIB)
            {
                continue;
            }
            let (outcome, natural) = spill_under(&sql, &budgets(4096 * MIB, memory, 2));
            let outcome: String = outcome.chars().take(160).collect();
            eprintln!(
                "shape={shape} memory={}MiB uncapped peak_len={} samples={} {outcome}",
                memory / MIB,
                natural.length,
                natural.samples,
            );
            if natural.length == 0 {
                continue;
            }
            let mut worst = i128::MIN;
            for percent in (20..=100).step_by(4) {
                let spill = (natural.length * percent / 100).max(BLOCK);
                let (outcome, peak) = spill_under(&sql, &budgets(spill, memory, 2));
                let outcome: String = outcome.chars().take(160).collect();
                let over = i128::from(peak.length.max(peak.allocated)) - i128::from(spill);
                worst = worst.max(over);
                eprintln!(
                    "  cap={spill} ({percent}%) peak_len={} peak_alloc={} over_bytes={over} \
                     samples={} {outcome}",
                    peak.length, peak.allocated, peak.samples,
                );
            }
            eprintln!(
                "shape={shape} memory={}MiB worst_over_bytes={worst}",
                memory / MIB
            );
        }
    }
}

/// `DuckDB`'s accounted spill usage and cap from a spill-cap failure's
/// message, `... (<used>/<cap> used)`: both in bytes, as far as the text
/// shows them, and the cap's text as written. See [`duckdb_format_bytes`]
/// for what the text loses.
fn accounted_usage(message: &str) -> Option<(u64, u64, &str)> {
    fn bytes(text: &str) -> Option<u64> {
        let (number, unit) = text.trim().split_once(' ')?;
        let unit = match unit {
            "bytes" => 1,
            "KiB" => 1024,
            "MiB" => MIB,
            "GiB" => 1024 * MIB,
            _ => return None,
        };
        let (whole, tenths) = number.split_once('.').unwrap_or((number, "0"));
        let tenths: u64 = whole.parse::<u64>().ok()? * 10 + tenths.parse::<u64>().ok()?;
        Some(tenths * unit / 10)
    }
    let inner = message.split_once(" (")?.1.split_once(" used)")?.0;
    let (used, cap) = inner.split_once('/')?;
    Some((bytes(used)?, bytes(cap)?, cap.trim()))
}

/// `DuckDB` 1.5.5's `StringUtil::BytesToHumanReadableString` for the units
/// the spill cap reaches. It names the largest unit that is at least one,
/// then writes one decimal from the next unit down, itself cut to whole
/// units: 202060595 bytes is 192 MiB and 716 KiB, so `192.6 MiB`, not
/// `192.7 MiB`. No tolerance covers that exactly, so the test compares the
/// text `DuckDB` printed with this rendering of the configured cap.
fn duckdb_format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["bytes", "KiB", "MiB", "GiB"];
    let mut parts = [0u64; 4];
    let mut rest = bytes;
    for part in &mut parts[..3] {
        *part = rest % 1024;
        rest /= 1024;
    }
    parts[3] = rest;
    for unit in (1..UNITS.len()).rev() {
        if parts[unit] > 0 {
            return format!(
                "{}.{} {}",
                parts[unit],
                parts[unit - 1] * 10 / 1024,
                UNITS[unit]
            );
        }
    }
    format!("{} bytes", parts[0])
}

/// The rendering matches what `DuckDB` printed for the cap that once failed
/// a tolerance check, and its other unit edges.
#[test]
fn duckdb_format_bytes_cuts_like_duckdb() {
    for (bytes, text) in [
        (202_060_595, "192.6 MiB"),
        (1023, "1023 bytes"),
        (1024, "1.0 KiB"),
        (MIB, "1.0 MiB"),
        (1024 * MIB, "1.0 GiB"),
    ] {
        assert_eq!(duckdb_format_bytes(bytes), text, "{bytes} bytes");
    }
}

/// The last cap of the sweep in [`spill_stays_within_budget_plus_class_slack`],
/// as a percentage of the natural peak.
const TOP_PERCENT: u64 = 96;

/// AC4's engine half (ADR-0056): a query that spills past the cap fails as
/// [`EngineError::ResourceExhausted`], the usage `DuckDB` accounts never
/// passes the cap, and the spill directory never holds more than the cap
/// plus [`CLASS_SLACK`].
///
/// The shape is a full sort over a million poorly compressible strings at
/// 128 MiB of memory and two threads, so it spills about 240 MiB with no
/// cap. The caps sweep from 40% to 96% of that natural peak: below about
/// half the sort fails while its runs are still growing, and above it
/// the merge frees and rewrites blocks, which is where the slack appears
/// (the probe's worst cases sat at 52%, 68% and 76%).
///
/// The directory is sampled with the query's process stopped
/// ([`spill_under`]): one `read_dir` pass over files that other threads
/// are growing and truncating is not one state of the directory, and its
/// sum can count a block twice. A stopped process gives a consistent sum,
/// so the assertion cannot fail on a sampling artefact. Sampling can miss
/// a peak between two samples, which weakens the check but never makes it
/// fail falsely.
#[test]
fn spill_stays_within_budget_plus_class_slack() {
    let data = tempfile::tempdir().expect("data");
    let source = random_strings_parquet(data.path(), 1_000_000);
    let sql = window_sort(&source);
    let memory = 128 * MIB;

    let (outcome, natural) = spill_under(&sql, &budgets(4096 * MIB, memory, 2));
    assert_eq!(outcome, "ok 1", "the uncapped sort completes");
    assert!(
        natural.length > 64 * MIB,
        "the uncapped sort must spill well past the smallest cap: {natural:?}"
    );

    let mut worst = i128::MIN;
    for percent in (40..=TOP_PERCENT).step_by(4) {
        let cap = natural.length * percent / 100;
        let (outcome, peak) = spill_under(&sql, &budgets(cap, memory, 2));
        // The natural peak is the largest spill the samples saw, and the sort's
        // real peak varies with thread timing. A cap in the last step can sit
        // above this run's real peak, and then the sort completes: correct
        // behaviour, so it keeps the directory bound and skips the message
        // checks. Every lower cap must stop the sort.
        if percent == TOP_PERCENT && outcome == "ok 1" {
            let held = peak.length.max(peak.allocated);
            assert!(
                held <= cap + CLASS_SLACK,
                "cap {cap} ({percent}%): the completed sort held {held} bytes ({peak:?})"
            );
            continue;
        }
        let message = outcome
            .strip_prefix("err resource_exhausted ")
            .unwrap_or_else(|| panic!("cap {cap} ({percent}%): {outcome}"));
        assert!(
            message.contains("failed to offload data block")
                && message.contains("max_temp_directory_size"),
            "cap {cap} ({percent}%): the spill cap, not memory, stopped the sort: {message}"
        );
        let (used, accounted_cap, cap_text) = accounted_usage(message)
            .unwrap_or_else(|| panic!("cap {cap} ({percent}%): no accounted usage in {message}"));
        assert_eq!(
            cap_text,
            duckdb_format_bytes(cap),
            "cap {cap} ({percent}%): DuckDB reports a different cap"
        );
        assert!(
            used <= accounted_cap,
            "cap {cap} ({percent}%): DuckDB accounted {used} past its cap {accounted_cap}"
        );
        let held = peak.length.max(peak.allocated);
        assert!(
            held <= cap + CLASS_SLACK,
            "cap {cap} ({percent}%): the spill directory held {held} bytes, {} past the cap \
             ({peak:?})",
            held - cap
        );
        worst = worst.max(i128::from(held) - i128::from(cap));
    }
    eprintln!(
        "natural spill {} bytes; worst overshoot past the cap {worst} bytes (block {BLOCK})",
        natural.length,
    );
}
