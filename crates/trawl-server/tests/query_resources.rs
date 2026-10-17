// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What trawld's queries may use, against the bundled `DuckDB` (ADR-0056):
//! the query database spills, snapshots and stages exports only inside
//! the query scratch directory, and a query that runs out of its spill cap
//! or memory limit answers 503 `query_resource_exhausted` with a fixed
//! sentence and nothing of `DuckDB`'s.
//!
//! A scenario that needs its own process runs in a CHILD PROCESS: this
//! test binary re-executes itself with the ignored
//! [`query_resources_child`] test selected, the scenario named in
//! [`CHILD_SCENARIO`] and a work directory the parent owns in
//! [`CHILD_WORK`]. The child boots a server under that directory, drives
//! it, and panics on a failed assertion; the parent asserts it exited
//! cleanly. Two reasons need one: a private `TMPDIR`, and sampling the
//! spill directory with the server's whole process stopped, so a sample is
//! one state of the files rather than a sum over files `DuckDB` grows and
//! truncates while the directory is read.
//!
//! Every server here installs the production subscriber
//! ([`telemetry::build_subscriber`]) with a stdout capture and a WAL
//! layer, as `http_failure` does, so a test can read what trawld logged
//! and stored about a request.

mod common;

use std::collections::HashSet;
use std::num::{NonZeroU32, NonZeroU64};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::util::SubscriberInitExt as _;
use trawl_engine::error::EngineError;
use trawl_server::bus::IngestBatch;
use trawl_server::config::RateLimitConfig;
use trawl_server::error::QUERY_RESOURCE_EXHAUSTED;
use trawl_server::ingest::producer::Derivation;
use trawl_server::ingest::wal::WalWriter;
use trawl_server::pool::seam::Seam;
use trawl_server::telemetry::{self, LogSinks, WalHandle, WalLayer};
use trawl_server::transport::failure::FAILURE_TARGET;

/// Set only in the child: which scenario to run.
const CHILD_SCENARIO: &str = "TRAWL_QUERY_RESOURCES_SCENARIO";

/// Set only in the child: the directory the parent made for it. The
/// child's server keeps its WAL, data root and query scratch here.
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
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    match scenario.to_str().expect("a UTF-8 scenario") {
        "tmpdir" => runtime.block_on(tmpdir_child(&work)),
        "spill_budget" => runtime.block_on(spill_budget_child(&work)),
        "shared_budget_generous" => runtime.block_on(shared_budget_generous_child(&work)),
        "shared_budget" => runtime.block_on(shared_budget_child(&work)),
        other => panic!("unknown scenario {other}"),
    }
}

/// Run `scenario` in a child process over `work`, with `env` added to this
/// process's environment, and fail with the child's output if it did not
/// pass.
fn run_child(scenario: &str, work: &Path, env: &[(&str, &Path)]) {
    finish_child(scenario, spawn_child(scenario, work, env));
}

/// Start `scenario` in a child process over `work`, with `env` added to
/// this process's environment.
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

/// Wait for a child [`spawn_child`] started, and fail with its output if
/// it did not pass.
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

/// Every entry directly in `dir`, by name.
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A hot-buffer batch of `n` events for `service`.
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

// -- AC9: nothing large in TMPDIR ---------------------------------------------

/// Hot-buffer snapshots and Parquet export staging files are made inside
/// the query scratch directory, and nothing trawld writes during a query,
/// an export or a snapshot lands in `TMPDIR` (ADR-0056).
///
/// The child runs with `TMPDIR` pointed at a private empty directory, and
/// its server keeps everything else under its work directory. The parent
/// checks the directory is still empty once the child is gone.
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

/// The child half of [`nothing_large_in_tmpdir`].
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

// -- the production subscriber, captured ------------------------------------

const ENV: &str = "default";

/// Everything the global subscriber writes to stdout.
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

/// The production subscriber under the default directives, installed once
/// for the process. Its WAL lives in the child's work directory when there
/// is one, so the subscriber writes nothing to `TMPDIR` either.
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

/// Every stdout line so far.
fn stdout_lines() -> Vec<String> {
    let bytes = sinks().stdout.0.lock().unwrap().clone();
    String::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Every record the telemetry WAL holds so far.
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

/// The persisted `event_type` records for one request.
fn records_for(event_type: &str, request_id: &str) -> Vec<serde_json::Value> {
    wal_records()
        .into_iter()
        .filter(|record| record["event_type"] == event_type && record["request_id"] == request_id)
        .collect()
}

/// A field's value on a formatted stdout line: `name="quoted"` or
/// `name=bare`.
fn field(line: &str, name: &str) -> Option<String> {
    let at = line.find(&format!(" {name}="))? + name.len() + 2;
    let rest = &line[at..];
    if let Some(quoted) = rest.strip_prefix('"') {
        quoted.find('"').map(|end| quoted[..end].to_owned())
    } else {
        Some(rest.split_whitespace().next().unwrap_or("").to_owned())
    }
}

// -- a corpus that spills -----------------------------------------------------

const MIB: u64 = 1024 * 1024;

/// What the spill directory may hold past the cap on `DuckDB` 1.5.5: one
/// block of each temp-file size class, 32 KiB to 256 KiB in 32 KiB steps
/// (ADR-0056; `CLASS_SLACK` in trawl-engine's `query_budgets` tests).
const CLASS_SLACK: u64 = 1_179_648;

/// Rows in [`spill_corpus`]. With 128 MiB of memory and two threads,
/// [`SPILLING`] spills about 70 MiB of them, in well under a second.
const CORPUS_ROWS: usize = 1_000_000;

/// A query whose answer is one row but whose window sorts every distinct
/// message: it must hold or spill the whole input.
const SPILLING: &str = "service=blob | dedup message | stats count()";

/// A query over the same corpus that needs next to nothing.
const SMALL: &str = "service=blob | stats count()";

/// `DuckDB` text that must never reach a response, a log line or a
/// stored record: its exception type, the settings its messages name, and
/// the units its sizes are written in.
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

/// The budgets a server under test runs with.
fn knobs(permits: usize, spill: u64, memory: u64) -> common::QueryKnobs {
    common::QueryKnobs {
        max_concurrent_queries: permits,
        spill_max_bytes: NonZeroU64::new(spill).unwrap(),
        memory_limit: NonZeroU64::new(memory).unwrap(),
        threads: NonZeroU32::new(2).unwrap(),
    }
}

/// A server over [`spill_corpus`] in `work`, with the production
/// subscriber installed.
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

/// One answer to a query: its status, request id and body.
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

/// `answer` is the 503 contract (ADR-0056): the code and the fixed
/// sentence, nothing else.
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

/// The `http_failure` trawld logged and stored for an exhausted request on
/// `route`: one record on each sink, a 503 `service_unavailable` with
/// cause `duckdb_resource_exhausted`.
fn assert_exhausted_failure(route: &str, request_id: &str) {
    let stored = records_for("http_failure", request_id);
    assert_eq!(stored.len(), 1, "one persisted failure: {stored:?}");
    let failure = &stored[0];
    assert_eq!(failure["target"], FAILURE_TARGET);
    assert_eq!(failure["status"], 503);
    assert_eq!(failure["route"], route);
    assert_eq!(failure["error_class"], "service_unavailable");
    assert_eq!(failure["cause_kind"], "duckdb_resource_exhausted");

    let lines: Vec<String> = stdout_lines()
        .into_iter()
        .filter(|line| field(line, "event_type").as_deref() == Some("http_failure"))
        .filter(|line| field(line, "request_id").as_deref() == Some(request_id))
        .collect();
    assert_eq!(lines.len(), 1, "one stdout failure: {lines:?}");
    assert_eq!(
        field(&lines[0], "cause_kind").as_deref(),
        Some("duckdb_resource_exhausted")
    );
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

/// Nothing trawld logged to stdout or stored since `mark` carries
/// `DuckDB` text or a size, or names the query scratch, and nothing about
/// the requests `request_ids` names any path under `work`. Boot records
/// that land late may name the data directory; they are not about a
/// query.
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

// -- the spill directory, sampled with the server stopped --------------------

/// The largest byte totals the spill directory reached across samples:
/// the sum of the files' lengths and of their allocated blocks.
#[derive(Debug, Default, Clone, Copy)]
struct Peak {
    length: u64,
    allocated: u64,
    samples: u64,
}

impl Peak {
    fn held(self) -> u64 {
        self.length.max(self.allocated)
    }
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

/// Run `scenario` in a child process over `work` and sample the spill
/// directory its server makes in `<work>/data.query-scratch` until it
/// exits, then fail with the child's output if it did not pass.
///
/// Each sample sends `SIGSTOP`, waits until every thread is stopped, reads
/// the directory, then sends `SIGCONT`. No thread moves a block while the
/// directory is read, so a sample is one consistent state of the files.
/// Sampling can still miss a peak between two samples, which weakens the
/// check but never makes it fail falsely.
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

// -- AC4: the spill cap -------------------------------------------------------

/// The spill cap of [`spill_budget`]: well under what [`SPILLING`] spills.
const SPILL_CAP: u64 = 24 * MIB;

/// A query that spills past `[server] query_spill_max_bytes` answers 503
/// `query_resource_exhausted` with cause `duckdb_resource_exhausted` and
/// the fixed sentence (ADR-0056). The spill directory never holds more
/// than the cap plus one block of each temp-file size class, the usage
/// `DuckDB` accounts never passes the cap, and a small query on the same
/// server then succeeds.
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

/// The child half of [`spill_budget`].
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
    assert_eq!(body["rows"], serde_json::json!([[CORPUS_ROWS]]));
    assert_nothing_leaked(work, &before, &[&answer.request_id, &small.request_id]);
}

// -- AC5: the memory limit ----------------------------------------------------

/// A query that runs out of `[server] query_memory_limit` answers the same
/// 503 contract, on the query lane and the export lane alike, and the
/// body, stdout and the stored `http_failure` carry no path, size or
/// `DuckDB` text (ADR-0056).
///
/// The spill cap is the shipped default, far past what the query needs,
/// and the spill directory stays empty: the memory limit stopped it.
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

/// A stored run whose read runs out of the query budgets answers the same
/// 503 contract, and nothing trawld logs about it names the stored file
/// (ADR-0056). The saved-run read runs on a clone of the query database,
/// so its budget failure is a query's, and a query's names no path.
///
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
        .claim_run(schedule, saved, SMALL, None, None)
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

// -- AC6: one budget, shared --------------------------------------------------

/// The memory limit of [`clones_share_one_spill_budget`]. Under it four
/// [`SPILLING`] queries at once all complete when spill is free; 128 MiB
/// stopped two of four on memory alone.
const SHARED_MEMORY: u64 = 256 * MIB;

/// The spill cap of [`clones_share_one_spill_budget`]. [`SPILLING`] alone
/// spills nothing under [`SHARED_MEMORY`]; four at once accounted 260 to
/// 340 MB of spill on `DuckDB` 1.5.5, about two to two and a half times
/// this cap.
const SHARED_CAP: u64 = 128 * MIB;

/// The spill cap of the calibration run, far past what four queries
/// spill.
const GENEROUS_CAP: u64 = 1024 * MIB;

/// Concurrent queries, one per permit, in [`clones_share_one_spill_budget`].
const SHARERS: usize = 4;

/// Concurrent queries on clones of the query database share its one
/// spill cap (ADR-0056). Four [`SPILLING`] queries started together all
/// complete under [`SHARED_MEMORY`] when spill is free, and `DuckDB`
/// accounts more spill for them than [`SHARED_CAP`]: memory alone does not
/// stop them. Under the cap one query still completes alone, but four
/// cannot all fit, so at least one answers 503 `query_resource_exhausted`,
/// where a cap per clone would let all four complete. The same statements
/// run raw on clones fail with `DuckDB`'s spill-cap message, not its
/// memory one. Neither the spill `DuckDB` accounts nor the spill directory
/// passes the one cap.
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

/// [`SHARERS`] [`SPILLING`] queries on `server`, every one started before
/// any runs, and the most spill `DuckDB` accounted while they ran.
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

/// A statement of [`SPILLING`]'s shape, written out over the file
/// [`spill_corpus`] wrote under `work`: the same window over every
/// distinct message, so it spills as the query does.
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

/// [`SHARERS`] copies of [`spilling_sql`] at once, each on its own clone
/// of `server`'s query database, with the failures `DuckDB` raised. A 503
/// names no cause, so this is where the test reads which budget stopped
/// the work.
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

/// The calibration half of [`clones_share_one_spill_budget`]: the same
/// work under the same memory limit, with spill to spare.
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

/// The capped half of [`clones_share_one_spill_budget`].
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

// -- AC8: the timeout verdict -------------------------------------------------

/// A query whose work fails with `Out of Memory` while its request times
/// out keeps its 504 (ADR-0056): the work's failure is parked until the
/// request has its answer, and does not replace it. `query_timing` records
/// the timeout and `query_permit_reclaimed` the work's failure, as for any
/// retained permit.
#[tokio::test(flavor = "multi_thread")]
async fn timeout_verdict_survives_resource_failure() {
    let work = tempfile::tempdir().expect("work dir");
    let server = spill_server(work.path(), 1, knobs(2, SPILL_CAP, 128 * MIB)).await;

    // The work runs into the spill cap within a few hundred milliseconds,
    // then parks with its failure in hand past the one-second deadline,
    // so the request answers 504 for work that has already failed. The
    // session releases its gates when dropped, so it lives as long as the
    // hold.
    let seams = server.state.query.pool.seams();
    let finished = seams.hold(Seam::Finished);
    let released = seams.watch(Seam::Released);
    let answer = post_query(&server, SPILLING).await;
    assert_eq!(
        finished.arrivals(),
        1,
        "the work failed before the deadline"
    );
    assert_eq!(answer.status, 504, "{}", answer.body);
    let body: serde_json::Value = serde_json::from_str(&answer.body).unwrap();
    assert_eq!(body["error"]["code"], "timeout");

    finished.release();
    let start = Instant::now();
    while released.arrivals() == 0 {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "the work never ended"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // The reclaim is logged as the worker hands back its permit.
    let start = Instant::now();
    let reclaimed = loop {
        let reclaimed = records_for("query_permit_reclaimed", &answer.request_id);
        if !reclaimed.is_empty() || start.elapsed() > Duration::from_secs(10) {
            break reclaimed;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
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

// -- guard ----------------------------------------------------------------------

/// No trawld production path opens a `DuckDB` database outside the query
/// database or makes a temp file in `TMPDIR` (ADR-0056).
///
/// `Executor::new` stays for embedded CLI mode and engine tests, and
/// spills into `TMPDIR` with no budgets, so trawld's sources may name it
/// only in test code. The scan drops `#[cfg(test)]` and test-support
/// items, then looks for the constructors.
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

/// Every `.rs` file under `dir`.
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
        // Skip every line up to and including the one holding the end.
        while i < lines.len() && starts[i] <= end {
            i += 1;
        }
    }
    (kept, gated_modules)
}

/// The byte offset of the character that ends the item starting at
/// `start` (see [`production_lines`]).
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
