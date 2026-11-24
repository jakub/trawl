// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What trawld's queries may use, against the bundled `DuckDB` (ADR-0056),
//! and what their regexes may hold (ADR-0063).
//!
//! A scenario runs in a CHILD PROCESS for a private `TMPDIR`, or to sample
//! the spill directory with the server's whole process stopped, so a sample
//! is one state of the files rather than a sum over files `DuckDB` grows and
//! truncates while the directory is read.

mod common;

use std::collections::HashSet;
use std::num::{NonZeroU32, NonZeroU64};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::util::SubscriberInitExt as _;
use trawl_core::parser::{ParseKind, ParseLog};
use trawl_core::pattern::{PatternKind, Preparer, charge, memory};
use trawl_engine::error::EngineError;
use trawl_engine::executor::prepare_probe;
use trawl_server::bus::{EventBus as _, IngestBatch};
use trawl_server::config::RateLimitConfig;
use trawl_server::error::{PREPARATION_ALLOWANCE_EXHAUSTED, QUERY_RESOURCE_EXHAUSTED};
use trawl_server::ingest::producer::Derivation;
use trawl_server::ingest::wal::WalWriter;
use trawl_server::pool::seam::{self, Seam};
use trawl_server::telemetry::{self, LogSinks, WalHandle, WalLayer};
use trawl_server::test_timing::{until, until_polling};
use trawl_server::transport::failure::FAILURE_TARGET;

const CHILD_SCENARIO: &str = "TRAWL_QUERY_RESOURCES_SCENARIO";

const CHILD_WORK: &str = "TRAWL_QUERY_RESOURCES_WORK";

/// The subprocess entry point. Without [`CHILD_SCENARIO`] it does nothing,
/// so a run with `--run-ignored` passes it untouched.
#[test]
#[ignore = "entry point for the child process; the tests below run it"]
fn query_resources_child() {
    let Some(scenario) = std::env::var_os(CHILD_SCENARIO) else {
        return;
    };
    let work = PathBuf::from(std::env::var_os(CHILD_WORK).expect("the parent names a work dir"));
    let scenario = scenario.to_str().expect("a UTF-8 scenario");
    // A paused clock needs a current-thread runtime.
    let mut builder = if matches!(
        scenario,
        "regex_reservation" | "regex_batch_re2" | "regex_fixed_sites" | "preparation_slot"
    ) {
        tokio::runtime::Builder::new_current_thread()
    } else {
        tokio::runtime::Builder::new_multi_thread()
    };
    let runtime = builder.enable_all().build().expect("a runtime");
    match scenario {
        "tmpdir" => runtime.block_on(tmpdir_child(&work)),
        "spill_budget" => runtime.block_on(spill_budget_child(&work)),
        "shared_budget_generous" => runtime.block_on(shared_budget_generous_child(&work)),
        "shared_budget" => runtime.block_on(shared_budget_child(&work)),
        "regex_reservation" => runtime.block_on(regex_reservation_child(&work)),
        "regex_batch_re2" => runtime.block_on(regex_batch_re2_child(&work)),
        "regex_fixed_sites" => runtime.block_on(regex_fixed_sites_child(&work)),
        "preparation_slot" => runtime.block_on(preparation_slot_child(&work)),
        other => panic!("unknown scenario {other}"),
    }
}

fn run_child(scenario: &str, work: &Path, env: &[(&str, &Path)]) {
    finish_child(scenario, spawn_child(scenario, work, env));
}

fn spawn_child(scenario: &str, work: &Path, env: &[(&str, &Path)]) -> Child {
    let mut command = Command::new(std::env::current_exe().expect("locate this test binary"));
    command
        .args([
            "query_resources_child",
            "--exact",
            "--ignored",
            "--test-threads=1",
            "--nocapture",
        ])
        .env(CHILD_SCENARIO, scenario)
        .env(CHILD_WORK, work);
    for (name, value) in env {
        command.env(name, value);
    }
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("start the child")
}

fn finish_child(scenario: &str, child: Child) {
    let output = child.wait_with_output().expect("wait for the child");
    assert!(
        output.status.success(),
        "the {scenario} child failed: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn hot_batch(id: &str, service: &str, n: usize) -> Arc<IngestBatch> {
    let events = (0..n)
        .map(|i| {
            let mut event = serde_json::Map::new();
            event.insert("_time".into(), "2026-01-03T00:05:00Z".into());
            event.insert("_ingested".into(), "2026-01-03T00:05:01Z".into());
            event.insert("service".into(), service.into());
            event.insert("message".into(), format!("hot event {i}").into());
            event
        })
        .collect();
    Arc::new(IngestBatch {
        batch_id: format!("prod/{service}_{id}").into(),
        service: service.into(),
        byte_size: 64 * n,
        events,
    })
}

#[test]
fn nothing_large_in_tmpdir() {
    let work = tempfile::tempdir().expect("work dir");
    let tmpdir = tempfile::tempdir().expect("private TMPDIR");
    run_child("tmpdir", work.path(), &[("TMPDIR", tmpdir.path())]);
    assert_eq!(
        entries(tmpdir.path()),
        Vec::<String>::new(),
        "nothing trawld wrote landed in TMPDIR"
    );
}

async fn tmpdir_child(work: &Path) {
    let tmpdir = std::env::temp_dir();
    assert_ne!(tmpdir, work, "the parent passed two directories");
    let empty = || {
        assert_eq!(entries(&tmpdir), Vec::<String>::new(), "TMPDIR stays empty");
    };
    empty();

    let server = common::setup_in_dir(work, RateLimitConfig::default()).await;
    let scratch = server.state.query.pool.scratch().path().to_owned();
    assert!(
        scratch.starts_with(work),
        "the default scratch is the data root's sibling: {}",
        scratch.display()
    );
    assert!(
        entries(&scratch)
            .iter()
            .any(|name| name.starts_with("trawl-spill-")),
        "the query database spills into scratch: {:?}",
        entries(&scratch)
    );
    empty();

    let hot = server
        .state
        .query
        .hot_buffer
        .clone()
        .expect("an ingest node has a hot buffer");
    hot.insert_for_test(hot_batch("1", "fresh", 20));
    let client = common::harness_client_builder().build().unwrap();

    // A query over the hot buffer, which builds a snapshot.
    let response = client
        .post(format!("{}/api/v1/query", server.url))
        .bearer_auth(&server.analyst_token)
        .json(&serde_json::json!({ "query": "service=fresh | stats count()" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{:?}", response.text().await);
    empty();

    // An export, held where it still holds its snapshot and its staging
    // file, on a new generation so it builds its own snapshot.
    hot.insert_for_test(hot_batch("2", "fresh", 20));
    let seams = server.state.query.pool.seams();
    let finished = seams.hold(Seam::Finished);
    let export = tokio::spawn({
        let (client, url, token) = (
            client.clone(),
            server.url.clone(),
            server.analyst_token.clone(),
        );
        async move {
            client
                .post(format!("{url}/api/v1/export?format=parquet"))
                .bearer_auth(token)
                .json(&serde_json::json!({ "query": "service=fresh" }))
                .send()
                .await
                .unwrap()
        }
    });
    let start = std::time::Instant::now();
    while finished.arrivals() == 0 {
        assert!(
            start.elapsed() < std::time::Duration::from_secs(30),
            "the export never finished its work"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let held = entries(&scratch);
    assert!(
        held.iter()
            .any(|name| name.starts_with("trawl-snapshot-") && name.ends_with(".ndjson")),
        "the snapshot is in scratch: {held:?}"
    );
    assert!(
        held.iter()
            .any(|name| name.starts_with("trawl-export-") && name.ends_with(".parquet")),
        "the export staging file is in scratch: {held:?}"
    );
    empty();
    finished.release();
    let response = export.await.unwrap();
    assert_eq!(response.status(), 200);
    let bytes = response.bytes().await.unwrap();
    assert!(bytes.starts_with(b"PAR1"), "a Parquet export");
    empty();
    assert!(
        !entries(&scratch)
            .iter()
            .any(|name| name.starts_with("trawl-export-")),
        "the staging file is gone once the export answered"
    );
}

const ENV: &str = "default";

#[derive(Clone, Default)]
struct Stdout(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Stdout {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

struct Sinks {
    stdout: Stdout,
    wal: WalLayer,
    wal_dir: tempfile::TempDir,
}

/// Its WAL lives in the child's work directory when there is one, so the
/// subscriber writes nothing to `TMPDIR` either.
fn sinks() -> &'static Sinks {
    static SINKS: OnceLock<Sinks> = OnceLock::new();
    SINKS.get_or_init(|| {
        let wal_dir = match std::env::var_os(CHILD_WORK) {
            Some(work) => tempfile::tempdir_in(work),
            None => tempfile::tempdir(),
        }
        .unwrap();
        let handle = WalHandle::new();
        handle.set(Arc::new(WalWriter::new(wal_dir.path().to_path_buf())), ENV);
        let wal = WalLayer::new_with_buffer_cap(
            handle,
            &[ENV.to_owned()],
            ENV,
            Arc::new(Derivation::defaults()),
            trawl_config::DEFAULT_TELEMETRY_BUFFER_MAX_BYTES,
        );
        let stdout = Stdout::default();
        let (subscriber, _) = telemetry::build_subscriber(
            telemetry::DEFAULT_LOG_FILTER,
            LogSinks {
                stdout_ansi: false,
                stdout: Some(stdout.clone()),
                wal: Some(wal.clone()),
                file_log: false,
            },
        );
        subscriber.init();
        Sinks {
            stdout,
            wal,
            wal_dir,
        }
    })
}

fn stdout_lines() -> Vec<String> {
    let bytes = sinks().stdout.0.lock().unwrap().clone();
    String::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn wal_records() -> Vec<serde_json::Value> {
    sinks().wal.flush();
    let Ok(entries) = std::fs::read_dir(sinks().wal_dir.path().join(ENV)) else {
        return Vec::new();
    };
    entries
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "ndjson"))
        .flat_map(|path| {
            std::fs::read_to_string(path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect::<Vec<_>>()
        })
        .collect()
}

fn records_for(event_type: &str, request_id: &str) -> Vec<serde_json::Value> {
    wal_records()
        .into_iter()
        .filter(|record| record["event_type"] == event_type && record["request_id"] == request_id)
        .collect()
}

fn field(line: &str, name: &str) -> Option<String> {
    let at = line.find(&format!(" {name}="))? + name.len() + 2;
    let rest = &line[at..];
    if let Some(quoted) = rest.strip_prefix('"') {
        quoted.find('"').map(|end| quoted[..end].to_owned())
    } else {
        Some(rest.split_whitespace().next().unwrap_or("").to_owned())
    }
}

const MIB: u64 = 1024 * 1024;

/// What the spill directory may hold past the cap on `DuckDB` 1.5.5
/// (ADR-0056).
const CLASS_SLACK: u64 = 1_179_648;

/// Rows in [`spill_corpus`]. With 128 MiB of memory and two threads,
/// [`SPILLING`] spills about 70 MiB of them, in well under a second.
const CORPUS_ROWS: usize = 1_000_000;

/// A query whose answer is one row but whose window sorts every distinct
/// message: it must hold or spill the whole input.
const SPILLING: &str = "service=blob | dedup message | stats count()";

const SMALL: &str = "service=blob | stats count()";

const DUCKDB_TEXT: [&str; 8] = [
    "Out of Memory",
    "out of memory",
    "max_temp_directory_size",
    "memory_limit",
    "failed to",
    "MiB",
    "KiB",
    "duckdb_temp",
];

/// Write [`CORPUS_ROWS`] events of 128-character hex messages, all
/// distinct, as one Parquet file in `data`'s on-disk layout. Row groups
/// stay small: the scan reads a column chunk whole, outside the memory
/// `DuckDB` can spill, and a default group of these strings would be the
/// allocation that fails.
fn spill_corpus(data: &Path) {
    let hour = data.join("prod").join("2026-01-02").join("10");
    std::fs::create_dir_all(&hour).unwrap();
    let conn = duckdb::Connection::open_in_memory().unwrap();
    conn.execute_batch(&format!(
        "COPY (SELECT TIMESTAMP '2026-01-02 10:00:00' + to_microseconds(i) AS _time, \
                TIMESTAMP '2026-01-02 10:00:01' + to_microseconds(i) AS _ingested, \
                NULL::VARCHAR AS _raw, NULL::VARCHAR AS _repairs, 'prod' AS env, \
                'blob' AS service, 'h' AS host, 9::BIGINT AS severity, \
                'info' AS severity_text, \
                md5(i::VARCHAR) || md5((i * 7)::VARCHAR) || md5((i * 13)::VARCHAR) \
                    || md5((i * 31)::VARCHAR) AS message \
         FROM range({CORPUS_ROWS}) t(i)) TO '{}' (FORMAT PARQUET, ROW_GROUP_SIZE 8192)",
        hour.join("blob.parquet").display()
    ))
    .unwrap();
}

fn knobs(permits: usize, spill: u64, memory: u64) -> common::QueryKnobs {
    common::QueryKnobs {
        max_concurrent_queries: permits,
        spill_max_bytes: NonZeroU64::new(spill).unwrap(),
        memory_limit: NonZeroU64::new(memory).unwrap(),
        threads: NonZeroU32::new(2).unwrap(),
    }
}

async fn spill_server(
    work: &Path,
    timeout_secs: u64,
    query: common::QueryKnobs,
) -> common::TestServer {
    sinks();
    let data = work.join("data");
    spill_corpus(&data);
    common::setup_with_query_knobs(work, data.to_str().unwrap().to_owned(), timeout_secs, query)
        .await
}

struct Answer {
    status: u16,
    request_id: String,
    body: String,
}

async fn post_query(server: &common::TestServer, dsl: &str) -> Answer {
    post(server, QUERY_ROUTE, dsl).await
}

/// A Parquet export, which stages its file in scratch before a byte goes
/// out, so a budget failure still answers with a status.
async fn post_export(server: &common::TestServer, dsl: &str) -> Answer {
    post(server, &format!("{EXPORT_ROUTE}?format=parquet"), dsl).await
}

const QUERY_ROUTE: &str = "/api/v1/query";
const EXPORT_ROUTE: &str = "/api/v1/export";

async fn post(server: &common::TestServer, path: &str, dsl: &str) -> Answer {
    let request = common::harness_client_builder()
        .build()
        .unwrap()
        .post(format!("{}{path}", server.url))
        .json(&serde_json::json!({ "query": dsl }));
    send(server, request).await
}

async fn get(server: &common::TestServer, path: &str) -> Answer {
    let request = common::harness_client_builder()
        .build()
        .unwrap()
        .get(format!("{}{path}", server.url));
    send(server, request).await
}

async fn send(server: &common::TestServer, request: reqwest::RequestBuilder) -> Answer {
    let response = request
        .bearer_auth(&server.analyst_token)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let request_id = response
        .headers()
        .get("x-request-id")
        .expect("every response carries X-Request-Id")
        .to_str()
        .unwrap()
        .to_owned();
    let body = response.text().await.unwrap();
    Answer {
        status,
        request_id,
        body,
    }
}

fn assert_exhausted(answer: &Answer) {
    assert_eq!(answer.status, 503, "{}", answer.body);
    let body: serde_json::Value = serde_json::from_str(&answer.body).unwrap();
    assert_eq!(
        body,
        serde_json::json!({
            "error": {
                "code": "query_resource_exhausted",
                "message": QUERY_RESOURCE_EXHAUSTED,
            }
        }),
        "the body is the code and the fixed sentence alone"
    );
}

fn assert_exhausted_failure(route: &str, request_id: &str) {
    assert_exhausted_cause(route, request_id, "duckdb_resource_exhausted");
}

fn assert_exhausted_cause(route: &str, request_id: &str, cause: &str) {
    let stored = records_for("http_failure", request_id);
    assert_eq!(stored.len(), 1, "one persisted failure: {stored:?}");
    let failure = &stored[0];
    assert_eq!(failure["target"], FAILURE_TARGET);
    assert_eq!(failure["status"], 503);
    assert_eq!(failure["route"], route);
    assert_eq!(failure["error_class"], "service_unavailable");
    assert_eq!(failure["cause_kind"], cause);

    let lines: Vec<String> = stdout_lines()
        .into_iter()
        .filter(|line| field(line, "event_type").as_deref() == Some("http_failure"))
        .filter(|line| field(line, "request_id").as_deref() == Some(request_id))
        .collect();
    assert_eq!(lines.len(), 1, "one stdout failure: {lines:?}");
    assert_eq!(field(&lines[0], "cause_kind").as_deref(), Some(cause));
}

/// Where both sinks stood before a test's requests: stdout is a stream,
/// so a line count marks it; the WAL is read file by file, in no promised
/// order, so the set of records already there marks it.
struct Mark {
    stdout: usize,
    stored: HashSet<String>,
}

fn mark() -> Mark {
    Mark {
        stdout: stdout_lines().len(),
        stored: wal_records().iter().map(ToString::to_string).collect(),
    }
}

/// Boot records that land late may name the data directory; they are not
/// about a query.
fn assert_nothing_leaked(work: &Path, mark: &Mark, request_ids: &[&str]) {
    let scratch = work.join("data.query-scratch");
    let scratch = scratch.to_str().unwrap();
    let work = work.to_str().unwrap();
    let lines = stdout_lines();
    let lines = &lines[mark.stdout..];
    let records: Vec<serde_json::Value> = wal_records()
        .into_iter()
        .filter(|record| !mark.stored.contains(&record.to_string()))
        .collect();
    for id in request_ids {
        assert!(
            lines
                .iter()
                .any(|line| field(line, "request_id").as_deref() == Some(id)),
            "request {id} logged nothing to stdout"
        );
        assert!(
            records.iter().any(|record| record["request_id"] == *id),
            "request {id} stored nothing"
        );
    }
    let stdout = lines.iter().map(|line| {
        let ours = field(line, "request_id").is_some_and(|id| request_ids.contains(&id.as_str()));
        ("stdout", line.clone(), ours)
    });
    let stored = records.iter().map(|record| {
        let ours = record["request_id"]
            .as_str()
            .is_some_and(|id| request_ids.contains(&id));
        ("stored", record.to_string(), ours)
    });
    for (sink, text, ours) in stdout.chain(stored) {
        for sentinel in DUCKDB_TEXT.iter().chain(&[scratch, "trawl-spill"]) {
            assert!(
                !text.contains(sentinel),
                "{sentinel:?} reached {sink}: {text}"
            );
        }
        assert!(
            !ours || !text.contains(work),
            "a path reached {sink}: {text}"
        );
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct Peak {
    length: u64,
    allocated: u64,
    samples: u64,
}

impl Peak {
    fn held(self) -> u64 {
        self.length
    }
}

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

/// Each sample sends `SIGSTOP`, waits until every thread is stopped, reads
/// the directory, then sends `SIGCONT`. Sampling can still miss a peak
/// between two samples, which weakens the check but never makes it fail
/// falsely.
fn spill_peak_of_child(scenario: &str, work: &Path) -> Peak {
    use rustix::process::{Pid, Signal, kill_process};

    let scratch = work.join("data.query-scratch");
    let mut child = spawn_child(scenario, work, &[]);
    let pid = Pid::from_child(&child);
    let mut peak = Peak::default();
    let mut spill = None;
    while child.try_wait().expect("child state").is_none() {
        if spill.is_none() {
            spill = std::fs::read_dir(&scratch).ok().and_then(|entries| {
                entries.flatten().map(|entry| entry.path()).find(|path| {
                    path.file_name()
                        .is_some_and(trawl_engine::scratch::is_spill_dir_name)
                })
            });
        }
        let Some(dir) = &spill else {
            std::thread::sleep(Duration::from_millis(1));
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
    finish_child(scenario, child);
    peak
}

/// The most spill `DuckDB` accounts for in its temp files while `run`
/// runs, read from `duckdb_temporary_files()` on a clone of the query
/// database that takes no permit.
async fn accounted_while<F: std::future::Future>(
    server: &common::TestServer,
    run: F,
) -> (F::Output, i64) {
    let executor = server
        .state
        .query
        .pool
        .detached_executor()
        .expect("a clone of the query database");
    let stop = Arc::new(AtomicBool::new(false));
    let most = Arc::new(AtomicI64::new(0));
    let sampler = std::thread::spawn({
        let (stop, most) = (Arc::clone(&stop), Arc::clone(&most));
        move || {
            while !stop.load(Ordering::SeqCst) {
                // The sampler draws on the same budget as the query it
                // watches, so a full budget refuses some samples.
                match executor.query_i64_for_test(
                    "SELECT coalesce(sum(size), 0)::BIGINT FROM duckdb_temporary_files()",
                ) {
                    Ok(used) => {
                        most.fetch_max(used, Ordering::SeqCst);
                    }
                    Err(EngineError::ResourceExhausted) => {}
                    Err(err) => panic!("read the temp files: {err}"),
                }
            }
        }
    });
    let output = run.await;
    stop.store(true, Ordering::SeqCst);
    sampler.join().unwrap();
    (output, most.load(Ordering::SeqCst))
}

const SPILL_CAP: u64 = 24 * MIB;

#[test]
fn spill_budget() {
    let work = tempfile::tempdir().expect("work dir");
    let peak = spill_peak_of_child("spill_budget", work.path());
    assert!(peak.samples > 0, "the spill directory was never sampled");
    assert!(
        peak.held() <= SPILL_CAP + CLASS_SLACK,
        "the spill directory held {} bytes, {} past the cap ({peak:?})",
        peak.held(),
        peak.held().saturating_sub(SPILL_CAP)
    );
    assert!(
        peak.held() >= SPILL_CAP / 2,
        "the query never spilled near the cap, so the cap is not what stopped it ({peak:?})"
    );
}

async fn spill_budget_child(work: &Path) {
    let server = spill_server(work, 60, knobs(2, SPILL_CAP, 128 * MIB)).await;
    let before = mark();

    let (answer, accounted) = accounted_while(&server, post_query(&server, SPILLING)).await;
    assert_exhausted(&answer);
    assert_exhausted_failure(QUERY_ROUTE, &answer.request_id);
    assert!(accounted > 0, "DuckDB accounted no spill at all");
    assert!(
        u64::try_from(accounted).unwrap() <= SPILL_CAP,
        "DuckDB accounted {accounted} bytes of spill past the cap {SPILL_CAP}"
    );

    let small = post_query(&server, SMALL).await;
    assert_eq!(small.status, 200, "{}", small.body);
    let body: serde_json::Value = serde_json::from_str(&small.body).unwrap();
    assert_eq!(body["rows"], serde_json::json!([[CORPUS_ROWS.to_string()]]));
    assert_nothing_leaked(work, &before, &[&answer.request_id, &small.request_id]);
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_limit() {
    let work = tempfile::tempdir().expect("work dir");
    let server = spill_server(
        work.path(),
        60,
        knobs(
            2,
            trawl_server::config::DEFAULT_QUERY_SPILL_MAX_BYTES.get(),
            8 * MIB,
        ),
    )
    .await;
    let before = mark();

    let answer = post_query(&server, SPILLING).await;
    assert_exhausted(&answer);
    assert_exhausted_failure(QUERY_ROUTE, &answer.request_id);

    let export = post_export(&server, SPILLING).await;
    assert_exhausted(&export);
    assert_exhausted_failure(EXPORT_ROUTE, &export.request_id);

    let small = post_query(&server, SMALL).await;
    assert_eq!(small.status, 200, "{}", small.body);
    assert_nothing_leaked(
        work.path(),
        &before,
        &[&answer.request_id, &export.request_id, &small.request_id],
    );
}

/// The run is planted with a result file whose one row group is a 16 MiB
/// column chunk, which the scan reads whole, outside what `DuckDB` can
/// spill, against an 8 MiB memory limit.
#[tokio::test(flavor = "multi_thread")]
async fn a_stored_run_read_past_the_budget_logs_no_path() {
    use trawl_server::store::{RunClaim, RunStatus};

    let work = tempfile::tempdir().expect("work dir");
    let server = spill_server(
        work.path(),
        60,
        knobs(
            2,
            trawl_server::config::DEFAULT_QUERY_SPILL_MAX_BYTES.get(),
            8 * MIB,
        ),
    )
    .await;
    let client = trawl_client::HttpClient::new_insecure(&server.url, &server.analyst_token)
        .expect("a client");
    let saved = client
        .create_saved("budgeted_run", SMALL)
        .await
        .expect("the saved query is created")
        .id;
    client
        .set_schedule(saved, "1h", None, true, None, None)
        .await
        .expect("the schedule is attached");
    let schedule: i64 = sqlx::query_scalar("SELECT id FROM schedules WHERE saved_query_id = $1")
        .bind(saved)
        .fetch_one(&common::app_pool(&server.app_db_url).await)
        .await
        .expect("the schedule the API just attached");
    let store = &server.state.storage.schedule;
    let run = match store
        .claim_run(schedule, saved, SMALL, None)
        .await
        .expect("claim a run")
    {
        RunClaim::Started(id) => id,
        other => panic!("expected a started run, got {other:?}"),
    };
    let relative = format!("scheduled/run_{run}.parquet");
    let stored = Path::new(server.state.query.pool.base_dir()).join(&relative);
    std::fs::create_dir_all(stored.parent().unwrap()).expect("the scheduled directory");
    duckdb::Connection::open_in_memory()
        .unwrap()
        .execute_batch(&format!(
            "COPY (SELECT md5(i::VARCHAR) || md5((i * 7)::VARCHAR) || md5((i * 13)::VARCHAR) \
                    || md5((i * 31)::VARCHAR) AS message \
             FROM range(131072) t(i)) TO '{}' (FORMAT PARQUET, ROW_GROUP_SIZE 131072)",
            stored.display()
        ))
        .unwrap();
    store
        .finish_run(
            run,
            RunStatus::Success,
            5,
            Some(131_072),
            None,
            None,
            Some(&relative),
        )
        .await
        .expect("finish the run");
    let before = mark();

    let answer = get(&server, &format!("/api/v1/saved/{saved}/runs/{run}")).await;
    assert_exhausted(&answer);

    let named: Vec<String> = stdout_lines()[before.stdout..]
        .iter()
        .filter(|line| line.contains(&relative))
        .cloned()
        .collect();
    assert!(
        named.is_empty(),
        "the stored file reached stdout: {named:#?}"
    );
    assert_nothing_leaked(work.path(), &before, &[&answer.request_id]);
}

/// The memory limit of [`clones_share_one_spill_budget`]. Under it four
/// [`SPILLING`] queries at once all complete when spill is free; 128 MiB
/// stopped two of four on memory alone.
const SHARED_MEMORY: u64 = 256 * MIB;

/// The spill cap of [`clones_share_one_spill_budget`]. [`SPILLING`] alone
/// spills nothing under [`SHARED_MEMORY`]; four at once accounted 260 to
/// 340 MB of spill on `DuckDB` 1.5.5, about two to two and a half times
/// this cap.
const SHARED_CAP: u64 = 128 * MIB;

const GENEROUS_CAP: u64 = 1024 * MIB;

const SHARERS: usize = 4;

#[test]
fn clones_share_one_spill_budget() {
    let generous = tempfile::tempdir().expect("work dir");
    run_child("shared_budget_generous", generous.path(), &[]);

    let work = tempfile::tempdir().expect("work dir");
    let peak = spill_peak_of_child("shared_budget", work.path());
    assert!(peak.samples > 0, "the spill directory was never sampled");
    assert!(
        peak.held() <= SHARED_CAP + CLASS_SLACK,
        "the spill directory held {} bytes, {} past the shared cap ({peak:?})",
        peak.held(),
        peak.held().saturating_sub(SHARED_CAP)
    );
    assert!(
        peak.held() >= SHARED_CAP / 2,
        "the queries never spilled near the cap, so the cap is not what stopped them ({peak:?})"
    );
}

async fn spill_together(server: &Arc<common::TestServer>) -> (Vec<Answer>, i64) {
    // Each parks at the work-start seam until the others have arrived.
    // The session releases its gates when dropped, so it lives as long as
    // the hold.
    let seams = server.state.query.pool.seams();
    let started = seams.hold(Seam::Started);
    let all = tokio::spawn({
        let server = Arc::clone(server);
        async move {
            let queries: Vec<_> = (0..SHARERS)
                .map(|_| {
                    let server = Arc::clone(&server);
                    tokio::spawn(async move { post_query(&server, SPILLING).await })
                })
                .collect();
            accounted_while(&server, async {
                let mut answers = Vec::with_capacity(SHARERS);
                for query in queries {
                    answers.push(query.await.unwrap());
                }
                answers
            })
            .await
        }
    });
    let start = Instant::now();
    while started.arrivals() < SHARERS {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "the queries never all started"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    started.release();
    all.await.unwrap()
}

fn spilling_sql(work: &Path) -> String {
    let corpus = work.join("data/prod/2026-01-02/10/blob.parquet");
    format!(
        "SELECT count(*) AS \"count\" FROM ( \
           SELECT * EXCLUDE (\"_rn\") FROM ( \
             SELECT *, ROW_NUMBER() OVER (PARTITION BY \"message\" \
               ORDER BY TRY_CAST(\"_time\" AS TIMESTAMP) DESC) AS \"_rn\" \
             FROM read_parquet('{}') WHERE \"service\" = 'blob') \
           WHERE \"_rn\" = 1)",
        corpus.display()
    )
}

/// A 503 names no cause, so this is where the test reads which budget
/// stopped the work.
fn spill_together_raw(server: &common::TestServer, work: &Path) -> Vec<Result<u64, duckdb::Error>> {
    let sql = spilling_sql(work);
    let barrier = Arc::new(std::sync::Barrier::new(SHARERS));
    let runs: Vec<_> = (0..SHARERS)
        .map(|_| {
            let executor = server
                .state
                .query
                .pool
                .detached_executor()
                .expect("a clone of the query database");
            let (barrier, sql) = (Arc::clone(&barrier), sql.clone());
            std::thread::spawn(move || {
                barrier.wait();
                executor.raw_count_rows_for_test(&sql)
            })
        })
        .collect();
    runs.into_iter().map(|run| run.join().unwrap()).collect()
}

async fn shared_budget_generous_child(work: &Path) {
    let server =
        Arc::new(spill_server(work, 60, knobs(SHARERS, GENEROUS_CAP, SHARED_MEMORY)).await);
    let (answers, accounted) = spill_together(&server).await;
    for answer in &answers {
        assert_eq!(
            answer.status, 200,
            "memory alone stopped a query: {}",
            answer.body
        );
    }
    assert!(
        u64::try_from(accounted).unwrap() > SHARED_CAP,
        "{SHARERS} queries accounted {accounted} bytes of spill at most, within the shared cap"
    );
    for outcome in spill_together_raw(&server, work) {
        outcome.unwrap_or_else(|e| panic!("memory alone stopped a raw statement: {e}"));
    }
}

async fn shared_budget_child(work: &Path) {
    let server = Arc::new(spill_server(work, 60, knobs(SHARERS, SHARED_CAP, SHARED_MEMORY)).await);

    let alone = post_query(&server, SPILLING).await;
    assert_eq!(alone.status, 200, "one query fits the cap: {}", alone.body);

    let (answers, accounted) = spill_together(&server).await;
    assert!(
        u64::try_from(accounted).unwrap() <= SHARED_CAP,
        "DuckDB accounted {accounted} bytes of spill past the shared cap"
    );
    let exhausted: Vec<&Answer> = answers
        .iter()
        .filter(|answer| answer.status != 200)
        .collect();
    assert!(
        !exhausted.is_empty(),
        "{SHARERS} queries that together spill past the cap all completed"
    );
    for answer in exhausted {
        assert_exhausted(answer);
        assert_exhausted_failure(QUERY_ROUTE, &answer.request_id);
    }

    let failures: Vec<String> = spill_together_raw(&server, work)
        .iter()
        .filter_map(|outcome| outcome.as_ref().err().map(ToString::to_string))
        .collect();
    assert!(
        !failures.is_empty(),
        "{SHARERS} raw statements that together spill past the cap all completed"
    );
    for message in &failures {
        assert!(
            message.contains("failed to offload data block")
                && message.contains("max_temp_directory_size"),
            "the spill cap, not memory, stopped the statement: {message}"
        );
    }
}

/// The deadline of the servers below, in their config's whole seconds.
const VERDICT_TIMEOUT_SECS: u64 = 1;

/// The tests that use it run on one `current_thread` runtime with tokio's
/// clock paused after boot. Every wait from the pause on goes through the
/// `test_timing` yield driver, which keeps the runtime busy so the paused
/// clock never auto-advances.
async fn park_spill_failure<F>(finished: &seam::Gate, request: &mut F)
where
    F: Future<Output = Answer> + Unpin,
{
    if let Some(early) = until_polling("the spilling work reaches Finished", request, || {
        finished.arrivals() == 1
    })
    .await
    {
        panic!(
            "the request answered {} before its work finished: {}",
            early.status, early.body
        );
    }
}

#[tokio::test]
async fn timeout_verdict_survives_resource_failure() {
    let work = tempfile::tempdir().expect("work dir");
    let server = spill_server(
        work.path(),
        VERDICT_TIMEOUT_SECS,
        knobs(2, SPILL_CAP, 128 * MIB),
    )
    .await;

    // The session releases its gates when dropped, so it lives as long as
    // the hold.
    tokio::time::pause();
    let seams = server.state.query.pool.seams();
    let finished = seams.hold(Seam::Finished);
    let released = seams.watch(Seam::Released);
    let mut request = std::pin::pin!(post_query(&server, SPILLING));
    park_spill_failure(&finished, &mut request).await;

    // Past the deadline and no further: every background timer of the
    // server due inside this window fires too, so the window stays the
    // smallest one that expires the request.
    tokio::time::advance(Duration::from_secs(VERDICT_TIMEOUT_SECS) + Duration::from_millis(1))
        .await;
    let answer = until_polling("the request answers", &mut request, || false)
        .await
        .expect("until_polling returns only once the request answers");
    assert_eq!(
        finished.arrivals(),
        1,
        "the work failed before the deadline"
    );
    assert_eq!(answer.status, 504, "{}", answer.body);
    let body: serde_json::Value = serde_json::from_str(&answer.body).unwrap();
    assert_eq!(body["error"]["code"], "timeout");

    finished.release();
    until("the work released everything", || released.arrivals() > 0).await;
    until("the permit reclaim is recorded", || {
        !records_for("query_permit_reclaimed", &answer.request_id).is_empty()
    })
    .await;
    let reclaimed = records_for("query_permit_reclaimed", &answer.request_id);
    assert_eq!(reclaimed.len(), 1, "{reclaimed:?}");
    assert_eq!(
        reclaimed[0]["physical_outcome"], "failed",
        "the retained work failed on its budget: {}",
        reclaimed[0]
    );

    let timing = records_for("query_timing", &answer.request_id);
    assert_eq!(timing.len(), 1, "{timing:?}");
    assert_eq!(timing[0]["outcome"], "timeout", "{}", timing[0]);
    assert_eq!(timing[0]["work_started"], true, "{}", timing[0]);

    let failures = records_for("http_failure", &answer.request_id);
    assert_eq!(failures.len(), 1, "one failure, the timeout: {failures:?}");
    assert_eq!(failures[0]["status"], 504);
    assert_eq!(failures[0]["error_class"], "timeout");
    assert!(
        wal_records()
            .iter()
            .all(|record| record["cause_kind"] != "duckdb_resource_exhausted"),
        "nothing recorded the late failure as the request's answer"
    );
}

/// The control for [`timeout_verdict_survives_resource_failure`]: the 504
/// there comes from the deadline passing first, not from the hold.
#[tokio::test]
async fn resource_failure_released_before_the_deadline_is_the_answer() {
    let work = tempfile::tempdir().expect("work dir");
    let server = spill_server(
        work.path(),
        VERDICT_TIMEOUT_SECS,
        knobs(2, SPILL_CAP, 128 * MIB),
    )
    .await;

    tokio::time::pause();
    let seams = server.state.query.pool.seams();
    let finished = seams.hold(Seam::Finished);
    let released = seams.watch(Seam::Released);
    let mut request = std::pin::pin!(post_query(&server, SPILLING));
    park_spill_failure(&finished, &mut request).await;
    let paused_at = tokio::time::Instant::now();

    finished.release();
    let answer = until_polling("the request answers", &mut request, || false)
        .await
        .expect("until_polling returns only once the request answers");
    assert_eq!(
        tokio::time::Instant::now(),
        paused_at,
        "the clock stood still, so the deadline never passed"
    );
    assert_exhausted(&answer);
    assert_exhausted_failure(QUERY_ROUTE, &answer.request_id);
    assert_eq!(released.arrivals(), 1, "the work released before answering");
}

const STREAM_ROUTE: &str = "/api/v1/stream";

/// Seven bodies of the `(\pL{12})` class, each near the 512 KiB batch
/// compiled-size limit. One query's regex budget holds six and refuses
/// the seventh.
const HEAVY: [&str; 7] = [
    r"(\pL{12})0",
    r"(\pL{12})1",
    r"(\pL{12})2",
    r"(\pL{12})3",
    r"(\pL{12})4",
    r"(\pL{12})5",
    r"(\pL{12})6",
];

const TAIL_PATTERN: &str = r"event \d+";

async fn regex_server(work: &Path, timeout_secs: u64) -> common::TestServer {
    regex_server_with(work, timeout_secs, common::QueryKnobs::DEFAULT).await
}

async fn regex_server_with(
    work: &Path,
    timeout_secs: u64,
    knobs: common::QueryKnobs,
) -> common::TestServer {
    sinks();
    let data = work.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let server = common::setup_with_query_knobs(
        work,
        data.to_str().unwrap().to_owned(),
        timeout_secs,
        knobs,
    )
    .await;
    server
        .state
        .query
        .hot_buffer
        .clone()
        .expect("an ingest node has a hot buffer")
        .insert_for_test(hot_batch("1", "fresh", 20));
    server
}

async fn get_stream(server: &common::TestServer, dsl: &str) -> Answer {
    let request = common::harness_client_builder()
        .build()
        .unwrap()
        .get(format!("{}{STREAM_ROUTE}", server.url))
        .query(&[("query", dsl)]);
    send(server, request).await
}

/// A live stream of `dsl` once its head is in. The body is never read, so
/// the stream stays open until the response drops.
async fn open_stream(server: &common::TestServer, dsl: &str) -> reqwest::Response {
    common::harness_client_builder()
        .build()
        .unwrap()
        .get(format!("{}{STREAM_ROUTE}", server.url))
        .query(&[("query", dsl)])
        .bearer_auth(&server.analyst_token)
        .send()
        .await
        .unwrap()
}

fn matches_stages(bodies: &[&str]) -> String {
    bodies.iter().fold(String::new(), |mut stages, body| {
        stages.push_str(" | where message matches /");
        stages.push_str(body);
        stages.push('/');
        stages
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn regex_budget_refusal_is_typed() {
    let work = tempfile::tempdir().expect("work dir");
    let server = regex_server(work.path(), common::DEFAULT_TEST_TIMEOUT_SECS).await;

    let in_filter = format!("service=fresh message=/{}/", HEAVY.join("/,/"));
    let in_stage = format!("service=fresh{}", matches_stages(&HEAVY));
    let in_tail = format!("service=fresh | extract kv{}", matches_stages(&HEAVY));

    let mut refused = Vec::new();
    for dsl in [&in_filter, &in_stage] {
        let answer = get_stream(&server, dsl).await;
        assert_exhausted(&answer);
        assert_exhausted_cause(STREAM_ROUTE, &answer.request_id, "regex_budget_exhausted");
        refused.push(answer.request_id);
    }
    let answer = post_query(&server, &in_tail).await;
    assert_exhausted(&answer);
    assert_exhausted_cause(QUERY_ROUTE, &answer.request_id, "regex_budget_exhausted");
    refused.push(answer.request_id);

    for request_id in &refused {
        for record in records_for("http_failure", request_id) {
            let record = record.to_string();
            assert!(
                !record.contains("pL{"),
                "a record quotes a pattern: {record}"
            );
        }
    }

    let ordinary = post_query(
        &server,
        &format!(
            "service=fresh | extract kv{}",
            matches_stages(&[TAIL_PATTERN])
        ),
    )
    .await;
    assert_eq!(ordinary.status, 200, "{}", ordinary.body);
}

/// What one stream or one batch tail over [`TAIL_PATTERN`] charges, as a
/// fresh ledger charges it. The pattern is dropped, so nothing holds
/// after.
fn tail_charge() -> u64 {
    let pattern = Preparer::new()
        .build(PatternKind::Regex, TAIL_PATTERN)
        .expect("the pattern builds");
    u64::try_from(pattern.charge().total()).unwrap()
}

/// The child lowers the reservation to one tail's charge and a build
/// transient, less a byte, so while one stream or tail holds its charge no
/// other build can start.
#[test]
fn regex_reservation_follows_physical_work() {
    let work = tempfile::tempdir().expect("work dir");
    run_child("regex_reservation", work.path(), &[]);
}

#[allow(clippy::too_many_lines)] // one scenario, its steps in order
async fn regex_reservation_child(work: &Path) {
    let reservation = memory::reservation();
    let transient = u64::try_from(charge::BUILD_TRANSIENT).unwrap();
    let charge = tail_charge();
    assert!(charge < transient, "a tail charges {charge} bytes");
    memory::set_reservation_limit(charge + transient - 1);

    let server = regex_server(work, VERDICT_TIMEOUT_SECS).await;
    let live = format!("service=fresh{}", matches_stages(&[TAIL_PATTERN]));
    let tail = format!(
        "service=fresh | extract kv{}",
        matches_stages(&[TAIL_PATTERN])
    );

    let stream = open_stream(&server, &live).await;
    assert_eq!(stream.status(), 200);
    assert_eq!(
        reservation.reserved(),
        charge,
        "the open stream holds its charge"
    );

    let refused = get_stream(&server, &live).await;
    assert_exhausted(&refused);
    assert_exhausted_cause(
        STREAM_ROUTE,
        &refused.request_id,
        "regex_reservation_exhausted",
    );
    let refused = post_query(&server, &tail).await;
    assert_exhausted(&refused);
    assert_exhausted_cause(
        QUERY_ROUTE,
        &refused.request_id,
        "regex_reservation_exhausted",
    );
    assert_eq!(reservation.reserved(), charge, "a refusal holds nothing");

    // Events on the bus make the stream write, so the server finds the
    // connection closed even if it never read the close.
    drop(stream);
    let bus = server
        .state
        .ingest
        .event_bus
        .clone()
        .expect("an ingest node has a bus");
    let (mut sent, mut last) = (0_u32, Instant::now());
    until("the closed stream returns its charge", || {
        if last.elapsed() > Duration::from_millis(20) {
            sent += 1;
            bus.publish(hot_batch(&format!("bus{sent}"), "fresh", 1));
            last = Instant::now();
        }
        reservation.reserved() == 0
    })
    .await;
    assert_eq!(reservation.holds(), reservation.releases());

    let seams = server.state.query.pool.seams();
    let released = seams.watch(Seam::Released);
    let pause = reservation.pause_after_charge();
    tokio::time::pause();
    let mut request = std::pin::pin!(post_query(&server, &tail));
    if let Some(early) = until_polling("the tail holds its charge", &mut request, || {
        pause.arrivals() == 1
    })
    .await
    {
        panic!(
            "the tail answered {} before its charge: {}",
            early.status, early.body
        );
    }
    assert_eq!(reservation.reserved(), charge);

    tokio::time::advance(Duration::from_secs(VERDICT_TIMEOUT_SECS) + Duration::from_millis(1))
        .await;
    let answer = until_polling("the request answers", &mut request, || false)
        .await
        .expect("until_polling returns only once the request answers");
    assert_eq!(answer.status, 504, "{}", answer.body);
    assert_eq!(released.arrivals(), 0, "the work has not ended");
    assert_eq!(
        reservation.reserved(),
        charge,
        "the timed-out work still holds its charge"
    );

    let mut second = std::pin::pin!(post_query(&server, &tail));
    let refused = until_polling("the second tail answers", &mut second, || false)
        .await
        .expect("until_polling returns only once the request answers");
    assert_exhausted(&refused);
    assert_exhausted_cause(
        QUERY_ROUTE,
        &refused.request_id,
        "regex_reservation_exhausted",
    );
    assert_eq!(reservation.reserved(), charge);

    // The work exits once the gate opens, and its plan, dropped inside
    // the work, returns the charge before the worker releases its slot.
    let ended = released.arrivals();
    pause.release();
    until("the timed-out work ends", || released.arrivals() > ended).await;
    assert_eq!(reservation.reserved(), 0, "the work returned its charge");
    assert_eq!(reservation.holds(), reservation.releases());
    drop(pause);

    let mut again = std::pin::pin!(post_query(&server, &tail));
    let answer = until_polling("the tail answers", &mut again, || false)
        .await
        .expect("until_polling returns only once the request answers");
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(reservation.reserved(), 0);
    assert_eq!(reservation.holds(), reservation.releases());
}

/// Beside the child's hot buffer each stage's filter runs in both branches
/// of the hot+cold union, but a thread holds one branch's at a time, so a
/// stage is still one site.
fn sites_query(n: usize) -> String {
    let bodies: Vec<String> = (0..n).map(|i| format!(r"event {i}\d*")).collect();
    let bodies: Vec<&str> = bodies.iter().map(String::as_str).collect();
    format!("service=fresh{}", matches_stages(&bodies))
}

fn matching_allowance(n: u64, threads: NonZeroU32) -> u64 {
    memory::RegexSites {
        extract: 0,
        matching: n,
        fixed: 0,
    }
    .re2_allowance(threads)
}

#[test]
fn regex_reservation_counts_batch_re2() {
    let work = tempfile::tempdir().expect("work dir");
    run_child("regex_batch_re2", work.path(), &[]);
}

#[allow(clippy::too_many_lines)] // one scenario, its steps in order
async fn regex_batch_re2_child(work: &Path) {
    let reservation = memory::reservation();
    assert_eq!(reservation.limit(), memory::REGEX_RESERVATION_BYTES);
    // Three permits: two queries held at the gate and a third that reaches
    // its reservation.
    let knobs = common::QueryKnobs {
        max_concurrent_queries: 3,
        ..common::QueryKnobs::DEFAULT
    };
    let threads = knobs.threads;
    let server = regex_server_with(work, VERDICT_TIMEOUT_SECS, knobs).await;

    let answer = post_query(&server, "service=fresh | stats count()").await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!((reservation.holds(), reservation.reserved()), (0, 0));

    let one = matching_allowance(1, threads);
    let seams = server.state.query.pool.seams();
    let released = seams.watch(Seam::Released);
    let pause = reservation.pause_after_charge();
    // From here every wait goes through the yield driver. A bare await
    // idles the runtime while a bearer check waits on Postgres, and the
    // paused clock jumps past the fleet pool's acquire timeout: a 503.
    tokio::time::pause();
    let dsl = sites_query(1);
    let binds = prepare_probe::count();
    let mut request = std::pin::pin!(post_query(&server, &dsl));
    if let Some(early) = until_polling("the statement holds its allowance", &mut request, || {
        pause.arrivals() == 1
    })
    .await
    {
        panic!(
            "the query answered {} before its allowance: {}",
            early.status, early.body
        );
    }
    assert_eq!(reservation.reserved(), one, "one site on {threads} threads");
    assert_eq!(
        prepare_probe::count(),
        binds,
        "the allowance is held before the statement binds"
    );

    tokio::time::advance(Duration::from_secs(VERDICT_TIMEOUT_SECS) + Duration::from_millis(1))
        .await;
    let answer = until_polling("the request answers", &mut request, || false)
        .await
        .expect("until_polling returns only once the request answers");
    assert_eq!(answer.status, 504, "{}", answer.body);
    assert_eq!(released.arrivals(), 0, "the work has not ended");
    assert_eq!(
        reservation.reserved(),
        one,
        "the timed-out work still holds its allowance"
    );

    let ended = released.arrivals();
    pause.release();
    until("the timed-out work ends", || released.arrivals() > ended).await;
    assert_eq!(reservation.reserved(), 0, "the work returned its allowance");
    assert_eq!(reservation.holds(), reservation.releases());
    drop(pause);

    let per_cap = memory::QUERY_RESERVATION_CAP / matching_allowance(1, threads);
    assert!(matching_allowance(per_cap + 1, threads) > memory::QUERY_RESERVATION_CAP);
    let holds = reservation.holds();
    let over = sites_query(usize::try_from(per_cap).unwrap() + 1);
    let mut refused = std::pin::pin!(post_query(&server, &over));
    let refused = until_polling("the over-cap query answers", &mut refused, || false)
        .await
        .expect("until_polling returns only once the request answers");
    assert_exhausted(&refused);
    assert_exhausted_cause(QUERY_ROUTE, &refused.request_id, "regex_budget_exhausted");
    assert_eq!((reservation.holds(), reservation.reserved()), (holds, 0));

    let big = matching_allowance(per_cap, threads);
    assert!(big <= memory::QUERY_RESERVATION_CAP);
    assert!(2 * big <= memory::REGEX_RESERVATION_BYTES);
    assert!(3 * big > memory::REGEX_RESERVATION_BYTES);
    let big_dsl = sites_query(usize::try_from(per_cap).unwrap());
    let pause = reservation.pause_after_charge();
    let binds = prepare_probe::count();
    let mut held = std::pin::pin!(async {
        tokio::join!(post_query(&server, &big_dsl), post_query(&server, &big_dsl))
    });
    if let Some(early) = until_polling("two statements hold their allowances", &mut held, || {
        pause.arrivals() == 2
    })
    .await
    {
        panic!(
            "the held queries answered early: {} and {}",
            early.0.status, early.1.status
        );
    }
    assert_eq!(reservation.reserved(), 2 * big);
    assert_eq!(prepare_probe::count(), binds, "neither statement has bound");

    let mut third = std::pin::pin!(post_query(&server, &big_dsl));
    let refused = until_polling("the third query answers", &mut third, || false)
        .await
        .expect("until_polling returns only once the request answers");
    assert_exhausted(&refused);
    assert_exhausted_cause(
        QUERY_ROUTE,
        &refused.request_id,
        "regex_reservation_exhausted",
    );
    assert_eq!(reservation.reserved(), 2 * big, "a refusal holds nothing");

    pause.release();
    let (first, second) = until_polling("the held queries answer", &mut held, || false)
        .await
        .expect("until_polling returns only once both answer");
    assert_eq!(first.status, 200, "{}", first.body);
    assert_eq!(second.status, 200, "{}", second.body);
    assert!(
        prepare_probe::count() > binds,
        "both statements bound once released"
    );
    drop(pause);
    until("the held work returns its allowance", || {
        reservation.reserved() == 0
    })
    .await;
    assert_eq!(reservation.holds(), reservation.releases());
}

/// Three `sev()` calls nothing downstream reads. Their SQL binds 123 of
/// conform's fixed regex calls on any source, and this server's sources
/// conform no `SEVERITY` pin of their own:
/// `trawl-engine/tests/fixed_regex_sites.rs` holds the count against
/// `DuckDB`'s plans as its shape `a few sev()`.
const FEW_SEV: &str = "service=fresh | let a = sev(message), b = sev(host), c = sev(service)";

const FEW_SEV_FIXED: u64 = 123;

#[test]
fn fixed_regex_sites_take_their_allowance() {
    let work = tempfile::tempdir().expect("work dir");
    run_child("regex_fixed_sites", work.path(), &[]);
}

async fn regex_fixed_sites_child(work: &Path) {
    let reservation = memory::reservation();
    assert_eq!(reservation.limit(), memory::REGEX_RESERVATION_BYTES);
    let knobs = common::QueryKnobs::DEFAULT;
    let threads = knobs.threads;
    assert_eq!(threads.get(), 4, "the shipped query threads");
    let server = regex_server_with(work, common::DEFAULT_TEST_TIMEOUT_SECS, knobs).await;
    let fixed = |calls| {
        memory::RegexSites {
            extract: 0,
            matching: 0,
            fixed: calls,
        }
        .re2_allowance(threads)
    };

    let few = fixed(FEW_SEV_FIXED);
    assert!(few <= memory::QUERY_RESERVATION_CAP);
    let pause = reservation.pause_after_charge();
    let mut request = std::pin::pin!(post_query(&server, FEW_SEV));
    if let Some(early) = until_polling("the statement holds its allowance", &mut request, || {
        pause.arrivals() == 1
    })
    .await
    {
        panic!(
            "the query answered {} before its allowance: {}",
            early.status, early.body
        );
    }
    assert_eq!(
        reservation.reserved(),
        few,
        "{FEW_SEV_FIXED} fixed calls on {threads} threads"
    );
    pause.release();
    let answer = until_polling("the query answers", &mut request, || false)
        .await
        .expect("until_polling returns only once the request answers");
    assert_eq!(answer.status, 200, "{}", answer.body);
    drop(pause);
    until("the work returns its allowance", || {
        reservation.reserved() == 0
    })
    .await;
    assert_eq!(reservation.holds(), reservation.releases());

    let lets: Vec<String> = (0..1000).map(|i| format!("s{i} = sev(message)")).collect();
    let thousand = format!("service=fresh | let {}", lets.join(", "));
    assert!(fixed(1000 * 41) > memory::QUERY_RESERVATION_CAP);
    let holds = reservation.holds();
    let refused = post_query(&server, &thousand).await;
    assert_exhausted(&refused);
    assert_exhausted_cause(QUERY_ROUTE, &refused.request_id, "regex_budget_exhausted");
    assert_eq!((reservation.holds(), reservation.reserved()), (holds, 0));
}

#[test]
fn ordinary_queries_fit_the_reservation_together() {
    let five: Vec<String> = charge::LOG_REGEXES
        .iter()
        .take(5)
        .map(|body| format!("/{body}/"))
        .collect();
    let search = format!("message={}", five.join(","));
    let with_sev = format!("{search} | let s = sev(message) | where s >= 9");
    let mut pins = trawl_core::schema::FieldTypes::new();
    pins.insert("_severity", trawl_core::schema::CanonicalType::Severity);
    for (dsl, hot, fixed) in [
        (&search, false, 0),
        (&with_sev, false, 82),
        // Beside a hot buffer each kind weighs the hot+cold union's
        // heavier branch, the hot one: its conform, the `sev()` and the
        // filter's copy of it.
        (&search, true, 199),
        (&with_sev, true, 199 + 41 + 41),
    ] {
        let query = trawl_core::parser::parse(dsl).expect("the ordinary query parses");
        let anchor = trawl_core::context::EvalContext::capture();
        let emitted = if hot {
            trawl_core::emitter::emit_with_hot_source(
                &query,
                "/data/*.parquet",
                "/data/hot.ndjson",
                &pins,
                &pins,
                &trawl_core::emitter::InputColumns::unprobed(),
                anchor,
            )
        } else {
            trawl_core::emitter::emit(&query, "/data/*.parquet", anchor)
        }
        .expect("the ordinary query emits");
        assert_eq!(
            emitted.regex_sites,
            memory::RegexSites {
                extract: 0,
                matching: 5,
                fixed,
            },
            "{dsl}, hot {hot}"
        );
        let one = emitted
            .regex_sites
            .re2_allowance(trawl_server::config::DEFAULT_QUERY_THREADS);
        assert!(one <= memory::QUERY_RESERVATION_CAP, "{dsl}");
        let queries = u64::try_from(trawl_server::config::MAX_DEFAULT_CONCURRENT_QUERIES).unwrap();
        assert!(
            queries * one <= memory::REGEX_RESERVATION_BYTES,
            "{queries} ordinary queries hold {} bytes, over the reservation of {}: {dsl}, \
             hot {hot}",
            queries * one,
            memory::REGEX_RESERVATION_BYTES
        );
        println!(
            "{dsl:.40}…, hot {hot}: {fixed} fixed calls, {one} bytes a query, {} left of the \
             reservation by {queries}",
            memory::REGEX_RESERVATION_BYTES - queries * one
        );
    }
}

/// The preparation tests' requests time out after this long: long enough
/// that a loaded machine still gets a freed slot to its waiter in time.
const WAIT_TIMEOUT_SECS: u64 = 3;

const VALIDATE_ROUTE: &str = "/api/v1/validate";
const SAVED_ROUTE: &str = "/api/v1/saved";
const RESULTS_ROUTE: &str = "/api/v1/results";

/// The query each door prepares in the off-reactor tests: a search term
/// and a stage that both carry `marker`, so a [`ParseLog`] on it sees
/// every parse of this request and of no other.
fn marked(marker: &str) -> String {
    format!("{marker} | where message matches /{marker}(s)?/")
}

/// The parses `log` recorded, once each is shown to have run off this
/// thread. The tests that call it run on a current-thread runtime, where
/// this thread is the only reactor thread (ADR-0063), and the fixture's
/// server is spawned on it.
fn assert_off_the_reactor(door: &str, log: &ParseLog) -> Vec<(ThreadId, ParseKind)> {
    let parses = log.parses();
    assert!(!parses.is_empty(), "{door}: the query text was parsed");
    let here = std::thread::current().id();
    assert!(
        parses.iter().all(|(thread, _)| *thread != here),
        "{door}: a parse ran on the reactor: {parses:?}"
    );
    parses
}

async fn send_json(
    server: &common::TestServer,
    method: reqwest::Method,
    path: &str,
    body: &serde_json::Value,
) -> Answer {
    let request = common::harness_client_builder()
        .build()
        .unwrap()
        .request(method, format!("{}{path}", server.url))
        .json(body);
    send(server, request).await
}

fn assert_success(door: &str, answer: &Answer) {
    assert!(
        (200..300).contains(&answer.status),
        "{door}: {} {}",
        answer.status,
        answer.body
    );
}

// The MCP tools reach trawld's preparation through `/query` and
// `/validate`, so the `query` and `validate` tests here cover their
// authoritative preparation. Their local parses, which shape a tool's
// answer, are trawl-mcp's own, and its tests show those run on the
// blocking pool.

#[tokio::test(flavor = "current_thread")]
async fn preparation_runs_off_the_reactor_query() {
    let server = common::setup().await;
    let marker = "zz336_door_query";
    let log = ParseLog::watch(marker);
    let answer = post_query(&server, &marked(marker)).await;
    assert_success("/query", &answer);
    assert_off_the_reactor("/query", &log);
}

#[tokio::test(flavor = "current_thread")]
async fn preparation_runs_off_the_reactor_validate() {
    let server = common::setup().await;
    let marker = "zz336_door_validate";
    let log = ParseLog::watch(marker);
    let answer = post(&server, VALIDATE_ROUTE, &marked(marker)).await;
    assert_success("/validate", &answer);
    let body: serde_json::Value = serde_json::from_str(&answer.body).unwrap();
    assert_eq!(body["valid"], true, "{body}");
    assert_off_the_reactor("/validate", &log);
}

#[tokio::test(flavor = "current_thread")]
async fn preparation_runs_off_the_reactor_stream_setup() {
    let server = common::setup().await;
    let marker = "zz336_door_stream";
    let log = ParseLog::watch(marker);
    let stream = open_stream(&server, &marked(marker)).await;
    assert_eq!(stream.status(), 200);
    drop(stream);
    assert_off_the_reactor("stream setup", &log);
}

#[tokio::test(flavor = "current_thread")]
async fn preparation_runs_off_the_reactor_export() {
    let server = common::setup().await;
    let marker = "zz336_door_export";
    let log = ParseLog::watch(marker);
    let answer = post(
        &server,
        &format!("{EXPORT_ROUTE}?format=csv"),
        &marked(marker),
    )
    .await;
    assert_success("export", &answer);
    assert_off_the_reactor("export", &log);
}

#[tokio::test(flavor = "current_thread")]
async fn preparation_runs_off_the_reactor_saved_create() {
    let server = common::setup().await;
    let marker = "zz336_door_saved_create";
    let log = ParseLog::watch(marker);
    let answer = send_json(
        &server,
        reqwest::Method::POST,
        SAVED_ROUTE,
        &serde_json::json!({ "name": "door", "query": marked(marker) }),
    )
    .await;
    assert_success("saved create", &answer);
    assert_off_the_reactor("saved create", &log);
}

#[tokio::test(flavor = "current_thread")]
async fn preparation_runs_off_the_reactor_saved_update() {
    let server = common::setup().await;
    let created = send_json(
        &server,
        reqwest::Method::POST,
        SAVED_ROUTE,
        &serde_json::json!({ "name": "door", "query": "* | head 1" }),
    )
    .await;
    assert_success("saved create", &created);
    let created: serde_json::Value = serde_json::from_str(&created.body).unwrap();
    let id = created["id"].as_i64().expect("a saved query id");

    let marker = "zz336_door_saved_update";
    let log = ParseLog::watch(marker);
    let answer = send_json(
        &server,
        reqwest::Method::PUT,
        &format!("{SAVED_ROUTE}/{id}"),
        &serde_json::json!({ "query": marked(marker) }),
    )
    .await;
    assert_success("saved update", &answer);
    assert_off_the_reactor("saved update", &log);
}

#[tokio::test(flavor = "current_thread")]
async fn preparation_runs_off_the_reactor_capture() {
    let server = common::setup().await;
    let marker = "zz336_door_capture";
    let log = ParseLog::watch(marker);
    let answer = send_json(
        &server,
        reqwest::Method::POST,
        RESULTS_ROUTE,
        &serde_json::json!({
            "query": marked(marker),
            "window": { "earliest": "2024-01-16T10:00:00Z", "latest": "2024-01-16T10:01:00Z" },
        }),
    )
    .await;
    assert_success("capture", &answer);
    assert_off_the_reactor("capture", &log);
}

#[tokio::test(flavor = "current_thread")]
async fn preparation_runs_off_the_reactor_scheduler() {
    let server = common::setup().await;
    let client =
        trawl_client::HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let marker = "zz336_door_scheduler";
    let saved = client.create_saved("door", &marked(marker)).await.unwrap();
    client
        .set_schedule(saved.id, "1h", None, true, None, None)
        .await
        .unwrap();

    let log = ParseLog::watch(marker);
    let handles = trawl_server::scheduler::poll_and_execute(
        &server.state.storage.schedule,
        &server.state.auth.key_store,
        &server.state.query.pool,
        &trawl_server::config::SchedulerConfig::default(),
        WAIT_TIMEOUT_SECS,
        chrono::Utc::now() + chrono::Duration::seconds(1),
    )
    .await;
    assert_eq!(handles.len(), 1, "the schedule was due");
    for handle in handles {
        handle.await.expect("a scheduled run must not panic");
    }
    let runs = client
        .list_report_runs(saved.id, Some(10), None)
        .await
        .unwrap();
    assert_eq!(runs.runs.len(), 1, "the run was recorded: {:?}", runs.runs);
    assert_off_the_reactor("scheduler", &log);
}

#[test]
fn preparation_slot_follows_physical_work() {
    let work = tempfile::tempdir().expect("work dir");
    run_child("preparation_slot", work.path(), &[]);
}

async fn preparation_gauge(server: &common::TestServer) -> String {
    let mut scrape = std::pin::pin!(get(server, "/metrics"));
    let answer = until_polling("the scrape answers", &mut scrape, || false)
        .await
        .expect("until_polling returns only once the request answers");
    assert_eq!(answer.status, 200, "{}", answer.body);
    answer
        .body
        .lines()
        .find_map(|line| {
            line.strip_prefix(trawl_server::metrics::QUERY_PREPARATION_IN_USE)?
                .strip_prefix(' ')
                .map(str::to_owned)
        })
        .expect("the scrape reports the preparation gauge")
}

async fn preparation_slot_child(work: &Path) {
    let reservation = memory::reservation();
    let server = regex_server(work, VERDICT_TIMEOUT_SECS).await;
    let preparation = server.state.query.pool.preparation().clone();
    let live = format!("service=fresh{}", matches_stages(&[TAIL_PATTERN]));
    let charge = tail_charge();
    assert_eq!(reservation.reserved(), 0);

    let pause = reservation.pause_after_charge();
    tokio::time::pause();
    let mut request = std::pin::pin!(get_stream(&server, &live));
    if let Some(early) = until_polling("the setup holds its charge", &mut request, || {
        pause.arrivals() == 1
    })
    .await
    {
        panic!(
            "the setup answered {} before its charge: {}",
            early.status, early.body
        );
    }
    assert_eq!(preparation.in_use(), 1, "the job holds its slot");
    assert_eq!(reservation.reserved(), charge, "the job holds its charge");
    assert_eq!(preparation_gauge(&server).await, "1");

    tokio::time::advance(Duration::from_secs(VERDICT_TIMEOUT_SECS) + Duration::from_millis(1))
        .await;
    let answer = until_polling("the setup answers", &mut request, || false)
        .await
        .expect("until_polling returns only once the request answers");
    assert_eq!(answer.status, 504, "{}", answer.body);
    assert_eq!(preparation.retained(), 1, "the job outlived its request");
    assert_eq!(preparation.in_use(), 1, "the retained job keeps its slot");
    assert_eq!(preparation.reclaimed(), 0);
    assert_eq!(
        reservation.reserved(),
        charge,
        "the retained job keeps its charge"
    );
    assert_eq!(preparation_gauge(&server).await, "1");

    // The job sees its stop once the gate opens, and its closure exits:
    // its ledger drops inside it, then its slot comes back.
    pause.release();
    until("the retained job's slot comes back", || {
        preparation.reclaimed() == 1
    })
    .await;
    assert_eq!(preparation.in_use(), 0, "the slot came back");
    assert_eq!(preparation.retained(), 0);
    assert_eq!(reservation.reserved(), 0, "the charge came back");
    assert_eq!(
        reservation.holds(),
        reservation.releases(),
        "every hold was returned once"
    );
    assert_eq!(preparation_gauge(&server).await, "0");
    drop(pause);

    // Once each: nothing more comes back later, and a setup runs again
    // in the one slot and charge the job gave back.
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert_eq!(preparation.reclaimed(), 1, "reclaimed once");
    assert_eq!(reservation.holds(), reservation.releases());
    let mut again = std::pin::pin!(open_stream(&server, &live));
    let stream = until_polling("the stream opens", &mut again, || false)
        .await
        .expect("until_polling returns only once the request answers");
    assert_eq!(stream.status(), 200);
    assert_eq!(reservation.reserved(), charge);
    drop(stream);
}

async fn fill_the_allowance(
    server: &Arc<common::TestServer>,
    parked: &seam::Gate,
) -> Vec<tokio::task::JoinHandle<Answer>> {
    let size = server.state.query.pool.preparation().size();
    let holders = (0..size)
        .map(|_| {
            let server = Arc::clone(server);
            tokio::spawn(async move { post_query(&server, "service=fresh | head 1").await })
        })
        .collect();
    until("every preparation slot is held", || {
        parked.arrivals() == size
    })
    .await;
    assert_eq!(server.state.query.pool.preparation().in_use(), size);
    holders
}

#[tokio::test(flavor = "multi_thread")]
async fn preparation_waits_within_the_deadline() {
    let work = tempfile::tempdir().expect("work dir");
    let server = Arc::new(regex_server(work.path(), WAIT_TIMEOUT_SECS).await);
    let seams = server.state.query.pool.seams();
    let parked = seams.hold(Seam::Preparing);
    let holders = fill_the_allowance(&server, &parked).await;
    let size = holders.len();

    let asked = Instant::now();
    let refused = post_query(&server, "service=fresh | head 1").await;
    assert_eq!(refused.status, 503, "{}", refused.body);
    let body: serde_json::Value = serde_json::from_str(&refused.body).unwrap();
    assert_eq!(
        body,
        serde_json::json!({
            "error": {
                "code": "query_resource_exhausted",
                "message": PREPARATION_ALLOWANCE_EXHAUSTED,
            }
        }),
        "the body is the code and the fixed sentence alone"
    );
    assert_exhausted_cause(
        QUERY_ROUTE,
        &refused.request_id,
        "preparation_allowance_exhausted",
    );
    assert!(
        asked.elapsed() >= Duration::from_secs(WAIT_TIMEOUT_SECS),
        "it waited for its deadline: {:?}",
        asked.elapsed()
    );
    assert_eq!(
        parked.arrivals(),
        size,
        "the refused request's job never ran"
    );

    // A waiter whose slot frees inside its deadline runs. The holders'
    // jobs keep their slots until they leave the seam.
    for holder in holders {
        assert_eq!(holder.await.unwrap().status, 504, "the holder timed out");
    }
    let waiter = tokio::spawn({
        let server = Arc::clone(&server);
        async move { post_query(&server, "service=fresh | head 1").await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!waiter.is_finished(), "the waiter waits for a slot");
    assert_eq!(parked.arrivals(), size, "the waiter has no slot yet");
    parked.release();
    let answer = waiter.await.unwrap();
    assert_eq!(answer.status, 200, "{}", answer.body);
}

/// A job that parked its reactor thread would leave nothing to answer the
/// probes, or to release it, so a watchdog lets the jobs go after 20 s
/// and says it had to.
#[tokio::test(flavor = "current_thread")]
async fn health_answers_while_preparation_is_saturated() {
    let work = tempfile::tempdir().expect("work dir");
    let server = Arc::new(regex_server(work.path(), common::DEFAULT_TEST_TIMEOUT_SECS).await);
    let seams = server.state.query.pool.seams();
    let parked = seams.hold(Seam::Preparing);

    let (done, watched) = std::sync::mpsc::channel::<()>();
    let rescued = Arc::new(AtomicBool::new(false));
    let watchdog = std::thread::spawn({
        let parked = Arc::clone(&parked);
        let rescued = Arc::clone(&rescued);
        move || {
            if watched.recv_timeout(Duration::from_secs(20)).is_err() {
                rescued.store(true, Ordering::SeqCst);
                parked.release();
            }
        }
    });

    let holders = fill_the_allowance(&server, &parked).await;
    let size = holders.len();
    let health = get(&server, "/api/v1/health").await;
    assert_eq!(health.status, 200, "{}", health.body);
    let metrics = get(&server, "/metrics").await;
    assert_eq!(metrics.status, 200, "{}", metrics.body);
    assert!(
        metrics
            .body
            .lines()
            .any(|line| line
                == format!("{} {size}", trawl_server::metrics::QUERY_PREPARATION_IN_USE)),
        "the scrape reports every slot held"
    );
    assert!(
        !rescued.load(Ordering::SeqCst),
        "the probes answered only after the parked jobs were let go"
    );
    assert_eq!(parked.arrivals(), size, "the jobs are still parked");

    done.send(()).unwrap();
    watchdog.join().unwrap();
    parked.release();
    for holder in holders {
        assert_eq!(holder.await.unwrap().status, 200);
    }
}

/// How many parses of each kind `log` recorded, every one off the
/// reactor: (built, unbuilt).
fn kinds(door: &str, log: &ParseLog) -> (usize, usize) {
    let parses = assert_off_the_reactor(door, log);
    let built = parses
        .iter()
        .filter(|(_, kind)| *kind == ParseKind::Built)
        .count();
    (built, parses.len() - built)
}

/// A built parse builds the query's patterns, an unbuilt one reads its
/// grammar alone.
///
/// - `/query` with a pipe: admission (built), `source::select`
///   (unbuilt), the engine (built) and the severity presentation walk
///   (unbuilt).
/// - CSV and JSON export: the same four. Parquet: three, with no
///   presentation walk.
/// - A live stream: one at setup (admission; the live compile reuses its
///   query), and none per event.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::too_many_lines)] // one door after another
async fn parse_count_per_request() {
    let work = tempfile::tempdir().expect("work dir");
    let server = regex_server(work.path(), common::DEFAULT_TEST_TIMEOUT_SECS).await;
    let counted = |marker: &str| format!("service=fresh | where message matches /event|{marker}/");

    let marker = "zz336_count_query";
    let log = ParseLog::watch(marker);
    let answer = post_query(&server, &counted(marker)).await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(kinds("/query", &log), (2, 2), "{:?}", log.parses());

    for (format, expected) in [("csv", (2, 2)), ("json", (2, 2)), ("parquet", (2, 1))] {
        let marker = format!("zz336_count_export_{format}");
        let log = ParseLog::watch(&marker);
        let answer = post(
            &server,
            &format!("{EXPORT_ROUTE}?format={format}"),
            &counted(&marker),
        )
        .await;
        assert_eq!(answer.status, 200, "{format}: {}", answer.body);
        assert_eq!(
            kinds(&format!("{format} export"), &log),
            expected,
            "{format}: {:?}",
            log.parses()
        );
    }

    let marker = "zz336_count_stream";
    let log = ParseLog::watch(marker);
    let mut stream = open_stream(&server, &counted(marker)).await;
    assert_eq!(stream.status(), 200);
    assert_eq!(kinds("stream setup", &log), (1, 0), "{:?}", log.parses());

    let bus = server
        .state
        .ingest
        .event_bus
        .clone()
        .expect("an ingest node has a bus");
    for batch in 0..3 {
        bus.publish(hot_batch(&format!("count{batch}"), "fresh", 4));
    }
    let mut body = String::new();
    while body.matches("hot event").count() < 12 {
        let chunk = stream
            .chunk()
            .await
            .unwrap()
            .expect("the stream stays open");
        body.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert_eq!(
        kinds("stream events", &log),
        (1, 0),
        "no parse per event: {:?}",
        log.parses()
    );
}

/// `Executor::new` stays for embedded CLI mode and engine tests, and
/// spills into `TMPDIR` with no budgets, so trawld's sources may name it
/// only in test code.
#[test]
fn no_trawld_path_opens_an_unbudgeted_database_or_writes_tmpdir() {
    const FORBIDDEN: [&str; 6] = [
        "Executor::new(",
        "NamedTempFile::new(",
        "tempfile::tempfile(",
        "tempfile::tempdir(",
        ".tempfile()",
        "std::env::temp_dir(",
    ];
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let files: Vec<(PathBuf, String)> = rust_files(&src)
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path).unwrap();
            (path, text)
        })
        .collect();
    // Files of modules declared under a test gate (`#[cfg(test)] mod x;`)
    // are test code whole.
    let gated: Vec<PathBuf> = files
        .iter()
        .flat_map(|(path, text)| {
            let dir = match path.file_stem().and_then(|stem| stem.to_str()) {
                Some("lib" | "main" | "mod") => path.parent().unwrap().to_owned(),
                _ => path.with_extension(""),
            };
            production_lines(text)
                .1
                .into_iter()
                .map(move |module| dir.join(module))
        })
        .collect();
    let mut offenders = Vec::new();
    for (path, text) in &files {
        if gated
            .iter()
            .any(|module| path.starts_with(module) || *path == module.with_extension("rs"))
        {
            continue;
        }
        for (line_no, line) in production_lines(text).0 {
            for needle in FORBIDDEN {
                if names(line, needle) && !line.trim_start().starts_with("//") {
                    offenders.push(format!("{}:{line_no}: {}", path.display(), line.trim()));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "trawld production code opens an unbudgeted database or writes TMPDIR:\n{}",
        offenders.join("\n")
    );
}

/// Whether `line` holds `needle` as a whole path, not as the tail of a
/// longer name (`TokioExecutor::new(` is not `Executor::new(`).
fn names(line: &str, needle: &str) -> bool {
    line.match_indices(needle).any(|(at, _)| {
        needle.starts_with('.')
            || line[..at]
                .chars()
                .next_back()
                .is_none_or(|before| !(before.is_alphanumeric() || before == '_'))
    })
}

fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(rust_files(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    files
}

/// The lines of `text` outside items gated on `test` (`#[cfg(test)]`,
/// `#[cfg(any(test, ...))]`), numbered from 1.
///
/// A gated item runs from the line after its attributes to the end of the
/// item: the brace that closes its first block for a `mod`, `fn`, `impl`,
/// `struct`, `enum` or `trait`, otherwise the first `;` or `,` outside any
/// bracket, or the brace that closes a block opened outside any bracket.
/// String, character and comment text is skipped while matching, so a
/// brace inside a fixture string does not end an item.
///
/// Also returns the names of the modules declared out of line under such a
/// gate (`#[cfg(test)] mod x;`), whose files are test code whole.
fn production_lines(text: &str) -> (Vec<(usize, &str)>, Vec<String>) {
    let lines: Vec<&str> = text.lines().collect();
    let starts: Vec<usize> = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(at, _)| at + 1))
        .collect();
    let mut kept = Vec::new();
    let mut gated_modules = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim_start();
        if !(trimmed.starts_with("#[cfg(test)]") || trimmed.starts_with("#[cfg(any(test")) {
            kept.push((i + 1, lines[i]));
            i += 1;
            continue;
        }
        i += 1;
        while i < lines.len() && lines[i].trim_start().starts_with("#[") {
            i += 1;
        }
        if i == lines.len() {
            break;
        }
        if let Some(module) = lines[i]
            .trim()
            .strip_prefix("mod ")
            .and_then(|rest| rest.strip_suffix(';'))
        {
            gated_modules.push(module.to_owned());
        }
        let end = item_end(text, starts[i]);
        while i < lines.len() && starts[i] <= end {
            i += 1;
        }
    }
    (kept, gated_modules)
}

fn item_end(text: &str, start: usize) -> usize {
    let bytes = text.as_bytes();
    let head = text[start..].trim_start();
    let head = head
        .strip_prefix("pub(crate) ")
        .or_else(|| head.strip_prefix("pub "))
        .unwrap_or(head);
    let block_item = [
        "mod ",
        "fn ",
        "async fn ",
        "impl",
        "struct ",
        "enum ",
        "trait ",
    ]
    .iter()
    .any(|keyword| head.starts_with(keyword));
    let mut depth = 0_usize;
    let mut at = start;
    while at < bytes.len() {
        match bytes[at] {
            b'/' if bytes.get(at + 1) == Some(&b'/') => {
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                at += 2;
                while at + 1 < bytes.len() && !(bytes[at] == b'*' && bytes[at + 1] == b'/') {
                    at += 1;
                }
                at += 1;
            }
            b'r' if matches!(bytes.get(at + 1), Some(b'"' | b'#'))
                && (at == 0
                    || !(bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_')) =>
            {
                let hashes = bytes[at + 1..].iter().take_while(|&&b| b == b'#').count();
                let close: String = std::iter::once('"')
                    .chain("#".repeat(hashes).chars())
                    .collect();
                let body = at + 2 + hashes;
                at = body + text[body..].find(&close).expect("a closed raw string") + close.len()
                    - 1;
            }
            b'"' => {
                at += 1;
                while bytes[at] != b'"' {
                    at += if bytes[at] == b'\\' { 2 } else { 1 };
                }
            }
            // A character literal; a lifetime has no closing quote.
            b'\'' if bytes.get(at + 2) == Some(&b'\'') => at += 2,
            b'\'' if bytes.get(at + 1) == Some(&b'\\') => {
                at += 2;
                while bytes[at] != b'\'' {
                    at += 1;
                }
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' => depth -= 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return at;
                }
            }
            b';' | b',' if depth == 0 && !block_item => return at,
            b';' if depth == 0 => return at,
            _ => {}
        }
        at += 1;
    }
    bytes.len()
}
