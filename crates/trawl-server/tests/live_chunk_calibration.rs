// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The live chunk calibration of issue #336 (ADR-0063, plan ruling R14).
//!
//! Ignored: `scripts/evidence/issue-336/live-chunk.sh` runs it in release
//! and writes `docs/evidence/issue-336/live-chunk.txt`. It drives the real
//! live driver (`trawl_server::live`) over an in-process bus with a plan
//! the real preparer built, at chunk sizes 1, 4, 16, 64 and 256 events,
//! each with the driver's 256 KiB string-payload cap.
//!
//! Two workloads. Mostly rejected: short log lines, 1 in 100 matching one
//! regex. Long subjects: 65,536-character values, each searched by three
//! admitted heavy regexes, every one of which scans to the value's end.
//!
//! Three phases a workload, each size in turn:
//! - throughput: a backlog is published and the bus closed, and the
//!   stream is read to its end. Events/s is the median of the rounds, and
//!   each job's duration is pooled over them.
//! - latency: batches are published on a fixed schedule at half the
//!   workload's throughput plateau for a fixed time, and each data frame's
//!   arrival is timed against its event's publish.
//! - stop: with a backlog under way, the stream is dropped, and the time
//!   until its SSE slot comes back (its job has exited) is measured.
//!
//! It prints the table, and writes it to the file `LIVE_CHUNK_REPORT`
//! names when that is set, and applies the rule: the smallest size within 10%
//! of the throughput plateau on both workloads, whose p99 job time is at
//! most 5 ms on mostly rejected events and whose publish-to-frame p95 is at
//! most 250 ms on long subjects. With no such size it names the bounds
//! each size failed, and the size is 64. The run fails, after it writes the
//! report, when that size is not `LIVE_CHUNK_EVENTS`.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "durations and rates for a printed table"
)]

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use tokio::sync::Semaphore;
use tokio_stream::StreamExt as _;
use trawl_core::cancel::Stop;
use trawl_core::filter::CompiledFilter;
use trawl_core::pattern::Preparer;
use trawl_core::pin_scope::PinScope;
use trawl_core::schema::FieldTypes;
use trawl_core::stream::{LIVE_CHUNK_EVENTS, StreamPlan, compile_stream_plan_with};
use trawl_server::bus::{EventBus, IngestBatch, LocalEventBus};
use trawl_server::live::seam::{JobTimes, calibration_frames};

const SIZES: [usize; 5] = [1, 4, 16, 64, 256];
const ROUNDS: usize = 3;
const LATENCY_FOR: Duration = Duration::from_secs(2);
/// How long the latency phase waits for the stream to drain after its
/// last publish. A frame still missing then counts past every bound.
const DRAIN_FOR: Duration = Duration::from_secs(10);
const STOP_TRIALS: usize = 10;

/// R14's size when no size meets every bound.
const FALLBACK: usize = 64;

const PLATEAU_SHARE: f64 = 0.9;
const JOB_P99_BOUND: Duration = Duration::from_millis(5);
const FRAME_P95_BOUND: Duration = Duration::from_millis(250);

/// Long subjects, in characters.
const SUBJECT: usize = 65_536;
/// The heavy bodies (`tests/query_resources.rs` `HEAVY`) one ledger admits.
const HEAVY: [&str; 3] = [r"\w{20}", r"(?i)\w{20}", r"\w{19}\w"];

struct Workload {
    name: &'static str,
    dsl: String,
    /// Distinct batches, published in turn, cyclically.
    pool: Vec<Arc<IngestBatch>>,
    /// The matching events of each pool batch.
    matches: Vec<usize>,
    /// Batches a throughput run publishes.
    backlog: usize,
}

impl Workload {
    fn events_of(&self, batches: usize) -> usize {
        (0..batches)
            .map(|i| self.pool[i % self.pool.len()].events.len())
            .sum()
    }

    fn matches_of(&self, batches: usize) -> usize {
        (0..batches)
            .map(|i| self.matches[i % self.matches.len()])
            .sum()
    }

    fn batch(&self, i: usize) -> Arc<IngestBatch> {
        Arc::clone(&self.pool[i % self.pool.len()])
    }
}

/// A deterministic word source.
struct Words(u64);

impl Words {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A word of `len` letters and digits.
    fn word(&mut self, len: usize) -> String {
        const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        (0..len)
            .map(|_| char::from(ALPHABET[(self.next() % ALPHABET.len() as u64) as usize]))
            .collect()
    }
}

fn ingest_batch(id: usize, events: Vec<Map<String, Value>>) -> Arc<IngestBatch> {
    Arc::new(IngestBatch {
        batch_id: format!("calibration-{id}").into(),
        service: "calibration".into(),
        events,
        byte_size: 0,
    })
}

fn object(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().expect("an object")
}

/// Short log lines in batches of 50, the 100th of every 100 matching.
fn mostly_rejected() -> Workload {
    let mut words = Words(0x9e37_79b9_7f4a_7c15);
    let (batches, per_batch) = (400, 50);
    let mut pool = Vec::with_capacity(batches);
    let mut matches = Vec::with_capacity(batches);
    for b in 0..batches {
        let mut hits = 0;
        let events = (0..per_batch)
            .map(|i| {
                let n = b * per_batch + i;
                let message = if n % 100 == 99 {
                    hits += 1;
                    format!(
                        "upstream {} timed out after {} ms while reading the response header",
                        words.word(8),
                        n % 5000
                    )
                } else {
                    format!(
                        "GET /api/v1/items/{n} 200 {}ms user={} trace={} agent=curl/8.11.1",
                        n % 97,
                        words.word(6),
                        words.word(16)
                    )
                };
                object(&json!({"n": n, "level": "info", "message": message}))
            })
            .collect();
        pool.push(ingest_batch(b, events));
        matches.push(hits);
    }
    Workload {
        name: "mostly rejected",
        dsl: r"message=/timed out after \d+ ms/".to_owned(),
        pool,
        matches,
        backlog: 40_000,
    }
}

/// 65,536-character values of words of 1 to 19 characters, ending in one
/// of 20, so each heavy regex scans to the end and every event matches.
/// Batches of 4 events.
fn long_subjects() -> Workload {
    let mut words = Words(0x2545_f491_4f6c_dd1d);
    let (batches, per_batch) = (16, 4);
    let mut pool = Vec::with_capacity(batches);
    for b in 0..batches {
        let events = (0..per_batch)
            .map(|i| {
                let mut message = String::with_capacity(SUBJECT);
                // Words of at most 19 and their spaces, then room for the
                // space and the 20-character word that ends the value.
                while message.len() + 20 <= SUBJECT - 21 {
                    let len = 1 + (words.next() % 19) as usize;
                    message.push_str(&words.word(len));
                    message.push(' ');
                }
                while message.len() < SUBJECT - 20 {
                    message.push(' ');
                }
                message.push_str(&words.word(20));
                assert_eq!(message.chars().count(), SUBJECT);
                object(&json!({"n": b * per_batch + i, "message": message}))
            })
            .collect();
        pool.push(ingest_batch(b, events));
    }
    let dsl = HEAVY
        .iter()
        .map(|body| format!("message=/{body}/"))
        .collect::<Vec<_>>()
        .join(" ");
    Workload {
        name: "long subjects",
        dsl,
        matches: vec![per_batch; batches],
        pool,
        backlog: 500,
    }
}

/// `dsl` prepared as a live stream's setup prepares it: one ledger under
/// the run's stop.
fn prepare(dsl: &str) -> (CompiledFilter, StreamPlan, Stop) {
    let stop = Stop::new();
    let query = trawl_core::parser::parse(dsl).expect("the query parses");
    let mut preparer = Preparer::new().with_stop(stop.clone());
    let filter = CompiledFilter::compile_with(&query.search, &FieldTypes::new(), &mut preparer)
        .expect("the filter is admitted");
    let plan =
        compile_stream_plan_with(&query.pipeline, &PinScope::unpinned(), None, &mut preparer)
            .expect("the plan is admitted");
    (filter, plan, stop)
}

/// A stream of `workload` at `size` events a job over `bus`, holding a
/// slot of `slots`.
fn open(
    workload: &Workload,
    bus: &LocalEventBus,
    slots: &Arc<Semaphore>,
    size: usize,
    times: &JobTimes,
) -> impl tokio_stream::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>
+ Send
+ 'static {
    let (filter, plan, stop) = prepare(&workload.dsl);
    let permit = Arc::clone(slots).try_acquire_owned().expect("a free slot");
    calibration_frames(
        bus.subscribe(),
        filter,
        plan,
        stop,
        permit,
        size,
        times.clone(),
    )
}

struct Throughput {
    events_per_s: f64,
    jobs: Vec<Duration>,
}

async fn throughput(workload: &Workload, size: usize) -> Throughput {
    let mut rates = Vec::with_capacity(ROUNDS);
    let mut jobs = Vec::new();
    for _ in 0..ROUNDS {
        let bus = LocalEventBus::new(workload.backlog.next_power_of_two());
        let slots = Arc::new(Semaphore::new(1));
        let times = JobTimes::default();
        let stream = open(workload, &bus, &slots, size, &times);
        for i in 0..workload.backlog {
            bus.publish(workload.batch(i));
        }
        drop(bus);
        let started = Instant::now();
        let frames = tokio::spawn(async move {
            let mut stream = std::pin::pin!(stream);
            let mut frames = 0;
            while stream.next().await.is_some() {
                frames += 1;
            }
            frames
        })
        .await
        .expect("the reader ran");
        let took = started.elapsed();
        assert_eq!(
            frames,
            workload.matches_of(workload.backlog),
            "one frame a match"
        );
        rates.push(workload.events_of(workload.backlog) as f64 / took.as_secs_f64());
        jobs.extend(times.take());
    }
    rates.sort_by(f64::total_cmp);
    jobs.sort();
    Throughput {
        events_per_s: rates[rates.len() / 2],
        jobs,
    }
}

struct Latency {
    offered: f64,
    frames: Vec<Duration>,
    /// Matches with no frame by the end of the drain.
    missing: usize,
}

async fn latency(workload: &Workload, size: usize, plateau: f64) -> Latency {
    let per_batch = workload.events_of(1) as f64;
    let offered = plateau / 2.0;
    let interval = Duration::from_secs_f64(per_batch / offered);
    let batches = (LATENCY_FOR.as_secs_f64() / interval.as_secs_f64()).ceil() as usize;
    let bus = LocalEventBus::new(batches.next_power_of_two());
    let slots = Arc::new(Semaphore::new(1));
    let stream = open(workload, &bus, &slots, size, &JobTimes::default());
    let reader = tokio::spawn(async move {
        let mut stream = std::pin::pin!(stream);
        let mut arrivals = Vec::new();
        while stream.next().await.is_some() {
            arrivals.push(Instant::now());
        }
        arrivals
    });

    // Every publish due by now goes out on each 1 ms tick.
    let mut published = Vec::with_capacity(batches);
    let mut tick = tokio::time::interval(Duration::from_millis(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let start = Instant::now();
    while published.len() < batches {
        tick.tick().await;
        let due =
            ((start.elapsed().as_secs_f64() / interval.as_secs_f64()) as usize + 1).min(batches);
        while published.len() < due {
            let i = published.len();
            published.push(Instant::now());
            bus.publish(workload.batch(i));
        }
    }
    drop(bus);
    let deadline = Instant::now() + DRAIN_FOR;
    let mut reader = reader;
    let arrivals = tokio::select! {
        arrivals = &mut reader => arrivals.expect("the reader ran"),
        () = tokio::time::sleep_until(deadline.into()) => {
            reader.abort();
            Vec::new()
        }
    };

    let expected: Vec<Instant> = published
        .iter()
        .enumerate()
        .flat_map(|(i, at)| std::iter::repeat_n(*at, workload.matches[i % workload.matches.len()]))
        .collect();
    let mut frames: Vec<Duration> = arrivals
        .iter()
        .zip(&expected)
        .map(|(arrived, at)| arrived.duration_since(*at))
        .collect();
    frames.sort();
    Latency {
        offered,
        missing: expected.len() - frames.len(),
        frames,
    }
}

/// Stop-to-exit over `STOP_TRIALS` streams, each dropped part way through
/// a backlog.
async fn stop_to_exit(workload: &Workload, size: usize, throughput: f64) -> Vec<Duration> {
    let backlog_for = workload.events_of(workload.backlog) as f64 / throughput;
    let mut exits = Vec::with_capacity(STOP_TRIALS);
    for trial in 0..STOP_TRIALS {
        let bus = LocalEventBus::new(workload.backlog.next_power_of_two());
        let slots = Arc::new(Semaphore::new(1));
        let stream = open(workload, &bus, &slots, size, &JobTimes::default());
        for i in 0..workload.backlog {
            bus.publish(workload.batch(i));
        }
        let reader = tokio::spawn(async move {
            let mut stream = std::pin::pin!(stream);
            while stream.next().await.is_some() {}
        });
        // 5 to 50 ms into a backlog that takes longer than that.
        let into = Duration::from_millis(5 * (1 + trial as u64));
        assert!(backlog_for > 0.1, "the backlog outlasts the stop");
        tokio::time::sleep(into).await;
        let stopped = Instant::now();
        reader.abort();
        let back = slots.acquire().await.expect("the slots stay open");
        exits.push(stopped.elapsed());
        drop(back);
        drop(bus);
    }
    exits.sort();
    exits
}

fn quantile(sorted: &[Duration], q: f64) -> Option<Duration> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    Some(sorted[rank - 1])
}

fn ms(duration: Option<Duration>) -> String {
    duration.map_or_else(
        || "-".to_owned(),
        |d| format!("{:.3}", d.as_secs_f64() * 1e3),
    )
}

struct Row {
    size: usize,
    throughput: Throughput,
    latency: Latency,
    exits: Vec<Duration>,
}

impl Row {
    /// The frame latency at `q`, or `None` when a frame that quantile
    /// covers never arrived.
    fn frame(&self, q: f64) -> Option<Duration> {
        let all = self.latency.frames.len() + self.latency.missing;
        let rank = ((q * all as f64).ceil() as usize).clamp(1, all.max(1));
        self.latency.frames.get(rank - 1).copied()
    }
}

async fn calibrate(workload: &Workload) -> Vec<Row> {
    let mut rows = Vec::with_capacity(SIZES.len());
    for size in SIZES {
        rows.push((size, throughput(workload, size).await));
    }
    let plateau = plateau(rows.iter().map(|(_, t)| t));
    let mut out = Vec::with_capacity(rows.len());
    for (size, throughput) in rows {
        let latency = latency(workload, size, plateau).await;
        let exits = stop_to_exit(workload, size, throughput.events_per_s).await;
        out.push(Row {
            size,
            throughput,
            latency,
            exits,
        });
    }
    out
}

fn plateau<'a>(rows: impl Iterator<Item = &'a Throughput>) -> f64 {
    rows.map(|t| t.events_per_s).fold(0.0, f64::max)
}

fn table(out: &mut String, workload: &Workload, rows: &[Row]) {
    let plateau = plateau(rows.iter().map(|r| &r.throughput));
    let offered = rows.first().map_or(0.0, |r| r.latency.offered);
    let per_batch = workload.events_of(1);
    let _ = writeln!(out, "## {}", workload.name);
    let _ = writeln!(out, "query:     {}", workload.dsl);
    let _ = writeln!(
        out,
        "backlog:   {} events in batches of {per_batch}, {} matching; latency phase offers {:.0} events/s (half the plateau) for {} s",
        workload.events_of(workload.backlog),
        workload.matches_of(workload.backlog),
        offered,
        LATENCY_FOR.as_secs()
    );
    let _ = writeln!(
        out,
        "{:>5} {:>13} {:>8} {:>7} {:>10} {:>10} {:>11} {:>11} {:>8} {:>10} {:>10}",
        "size",
        "events/s",
        "plateau",
        "jobs",
        "job p50",
        "job p99",
        "frame p50",
        "frame p95",
        "unframed",
        "stop p50",
        "stop max"
    );
    for row in rows {
        let _ = writeln!(
            out,
            "{:>5} {:>13.0} {:>7.1}% {:>7} {:>10} {:>10} {:>11} {:>11} {:>8} {:>10} {:>10}",
            row.size,
            row.throughput.events_per_s,
            100.0 * row.throughput.events_per_s / plateau,
            row.throughput.jobs.len(),
            ms(quantile(&row.throughput.jobs, 0.50)),
            ms(quantile(&row.throughput.jobs, 0.99)),
            ms(row.frame(0.50)),
            ms(row.frame(0.95)),
            row.latency.missing,
            ms(quantile(&row.exits, 0.50)),
            ms(row.exits.last().copied()),
        );
    }
    let _ = writeln!(out);
}

/// The bounds `size` fails, by name.
fn failures(rejected: &Row, long: &Row, rejected_plateau: f64, long_plateau: f64) -> Vec<String> {
    let mut failed = Vec::new();
    if rejected.throughput.events_per_s < PLATEAU_SHARE * rejected_plateau {
        failed.push("throughput under 90% of the plateau on mostly rejected events".to_owned());
    }
    if long.throughput.events_per_s < PLATEAU_SHARE * long_plateau {
        failed.push("throughput under 90% of the plateau on long subjects".to_owned());
    }
    if quantile(&rejected.throughput.jobs, 0.99).is_none_or(|p99| p99 > JOB_P99_BOUND) {
        failed.push("job p99 over 5 ms on mostly rejected events".to_owned());
    }
    if long.frame(0.95).is_none_or(|p95| p95 > FRAME_P95_BOUND) {
        failed.push("publish-to-frame p95 over 250 ms on long subjects".to_owned());
    }
    failed
}

/// Whether the run passes: the rule chose `constant`, or chose no size
/// while `constant` is the fallback. The error says why not.
fn verdict(chosen: Option<usize>, constant: usize) -> Result<(), String> {
    match chosen {
        Some(size) if size == constant => Ok(()),
        Some(size) => Err(format!(
            "the rule chose {size} events a job, but LIVE_CHUNK_EVENTS is {constant}"
        )),
        None if constant == FALLBACK => Ok(()),
        None => Err(format!(
            "no size meets every bound, so the size is {FALLBACK}, but LIVE_CHUNK_EVENTS is {constant}"
        )),
    }
}

/// The run fails when the rule rejects `LIVE_CHUNK_EVENTS`: it chose
/// another size, or chose none while the constant is not the fallback.
#[test]
fn the_run_fails_when_the_rule_rejects_the_constant() {
    assert_eq!(verdict(Some(256), 256), Ok(()));
    assert_eq!(verdict(None, FALLBACK), Ok(()));
    assert!(
        verdict(Some(64), 256).is_err(),
        "the rule chose another size"
    );
    assert!(
        verdict(None, 256).is_err(),
        "no size meets every bound, and the constant is not the fallback"
    );
}

#[test]
#[ignore = "the chunk calibration: scripts/evidence/issue-336/live-chunk.sh runs it in release"]
fn live_chunk_calibration() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let rejected = mostly_rejected();
    let long = long_subjects();
    let (rejected_rows, long_rows) = runtime.block_on(async {
        // One warm-up pass, untimed.
        let _ = throughput(&rejected, 64).await;
        (calibrate(&rejected).await, calibrate(&long).await)
    });

    let mut out = String::new();
    let _ = writeln!(
        out,
        "sizes {SIZES:?} events a job, string-payload cap 256 KiB a chunk; {ROUNDS} throughput rounds a size (events/s is their median, job times pooled); {STOP_TRIALS} stop trials a size"
    );
    let _ = writeln!(
        out,
        "times in ms; frame = publish to data frame; unframed = matches with no frame {} s after the last publish (they count past every bound); stop = stream dropped mid-backlog to its SSE slot released",
        DRAIN_FOR.as_secs()
    );
    let _ = writeln!(out);
    table(&mut out, &rejected, &rejected_rows);
    table(&mut out, &long, &long_rows);

    let rejected_plateau = plateau(rejected_rows.iter().map(|r| &r.throughput));
    let long_plateau = plateau(long_rows.iter().map(|r| &r.throughput));
    let _ = writeln!(
        out,
        "## the rule: the smallest size within 10% of the throughput plateau on both workloads, job p99 <= 5 ms on mostly rejected events, publish-to-frame p95 <= 250 ms on long subjects"
    );
    let mut chosen = None;
    for (rejected, long) in rejected_rows.iter().zip(&long_rows) {
        let failed = failures(rejected, long, rejected_plateau, long_plateau);
        if failed.is_empty() {
            let _ = writeln!(out, "size {:>3}: meets every bound", rejected.size);
            chosen.get_or_insert(rejected.size);
        } else {
            let _ = writeln!(
                out,
                "size {:>3}: fails: {}",
                rejected.size,
                failed.join("; ")
            );
        }
    }
    match chosen {
        Some(size) => {
            let _ = writeln!(out, "chosen: {size} events a job");
        }
        None => {
            let _ = writeln!(
                out,
                "chosen: none; no size meets every bound, so the size stays {FALLBACK}"
            );
        }
    }
    let _ = writeln!(out, "LIVE_CHUNK_EVENTS at this commit: {LIVE_CHUNK_EVENTS}");
    let verdict = verdict(chosen, LIVE_CHUNK_EVENTS);
    let _ = match &verdict {
        Ok(()) => writeln!(out, "verdict: pass; the rule agrees with LIVE_CHUNK_EVENTS"),
        Err(why) => writeln!(out, "verdict: FAIL; {why}"),
    };
    print!("{out}");
    if let Some(path) = std::env::var_os("LIVE_CHUNK_REPORT") {
        std::fs::write(path, out).expect("the report is written");
    }
    if let Err(why) = verdict {
        panic!("{why}");
    }
}
