// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What trawld's queries may use, against the bundled `DuckDB` (ADR-0056):
//! the query database spills, snapshots and stages exports only inside
//! the query scratch directory, and a query that runs out of its spill cap
//! or memory limit answers 503 `query_resource_exhausted` with a fixed
//! sentence and nothing of `DuckDB`'s.
//!
//! And what their regexes may hold (ADR-0063): a query over its regex
//! budget, or one the server-wide regex reservation cannot hold, answers
//! the same 503 with a regex cause, and the reservation is held by the
//! work that matches, not by the request that waits for it. A batch
//! statement holds an RE2 allowance from that reservation for every
//! regex `DuckDB` binds, on every thread.
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
use trawl_core::pattern::{PatternKind, Preparer, charge, memory};
use trawl_engine::error::EngineError;
use trawl_engine::executor::prepare_probe;
use trawl_server::bus::{EventBus as _, IngestBatch};
use trawl_server::config::RateLimitConfig;
use trawl_server::error::QUERY_RESOURCE_EXHAUSTED;
use trawl_server::ingest::producer::Derivation;
use trawl_server::ingest::wal::WalWriter;
use trawl_server::pool::seam::{self, Seam};
use trawl_server::telemetry::{self, LogSinks, WalHandle, WalLayer};
use trawl_server::test_timing::{until, until_polling};
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
    let scenario = scenario.to_str().expect("a UTF-8 scenario");
    // A paused clock needs a current-thread runtime.
    let mut builder = if matches!(
        scenario,
        "regex_reservation" | "regex_batch_re2" | "regex_fixed_sites"
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
/// (ADR-0056; `CLASS_SLACK` in trawl-engine's `query_budgets` tests). That
/// is one block per class only while each class stays within one temp file,
/// which holds at the small caps these tests use; at the 4 GiB default a
/// class can hold several files, each overshooting by up to one block.
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
    assert_exhausted_cause(route, request_id, "duckdb_resource_exhausted");
}

/// [`assert_exhausted_failure`] with cause `cause`.
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
/// than the cap plus one block of each temp-file size class (one file per
/// class, which holds at this test's small cap), the usage
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
    assert_eq!(body["rows"], serde_json::json!([[CORPUS_ROWS.to_string()]]));
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

/// The deadline of the servers below, in their config's whole seconds.
const VERDICT_TIMEOUT_SECS: u64 = 1;

/// Drive `request`, a [`SPILLING`] query, until its worker arrives at the
/// `finished` hold with its spill failure in hand, failing if the request
/// answers first.
///
/// The tests that use it run on one `current_thread` runtime with tokio's
/// clock paused after boot: the server's tasks and the request share the
/// thread, and the readiness probe's real timeout is behind them. Every
/// wait from the pause on goes through the `test_timing` yield driver,
/// which keeps the runtime busy so the paused clock never auto-advances.
/// The request deadline then moves only when a test advances it, however
/// long the spill takes on a loaded machine.
async fn park_spill_failure<F>(finished: &seam::Gate, request: &mut F)
where
    F: Future<Output = Answer> + Unpin,
{
    // Waits for: the worker's arrival at Finished.
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

/// A query whose work fails with `Out of Memory` while its request times
/// out keeps its 504 (ADR-0056): the work's failure is parked until the
/// request has its answer, and does not replace it. `query_timing` records
/// the timeout and `query_permit_reclaimed` the work's failure, as for any
/// retained permit.
///
/// The order is fixed by events, not by the spill finishing inside a
/// real second: the work parks at Finished with its failure, the clock
/// then moves just past the deadline, and the 504 must arrive while the
/// failure is still held.
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
    // Waits for: the request's answer, with the failed work still parked.
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
    // Waits for: the worker's arrival at Released, after its cleanup.
    until("the work released everything", || released.arrivals() > 0).await;
    // Waits for: the reclaim record, logged as the worker hands back its
    // permit.
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

/// The control for [`timeout_verdict_survives_resource_failure`]: the
/// same parked spill failure, released before the deadline, is the
/// request's answer, a 503 `query_resource_exhausted`. So the 504 there
/// comes from the deadline passing first, not from the hold.
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
    // Waits for: the request's answer. The clock is never advanced, so
    // the deadline cannot pass first.
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

// -- ADR-0063: regex memory ------------------------------------------------------

const STREAM_ROUTE: &str = "/api/v1/stream";

/// Four bodies of the `\w{20}` class, each near the 1 MiB compiled-size
/// limit. One query's regex budget holds three and refuses the fourth.
const HEAVY: [&str; 4] = [r"\w{20}", r"(?i)\w{20}", r"\w{19}\w", r"\w\w{19}"];

/// The pattern the reservation tests' streams and tails match the hot
/// events with.
const TAIL_PATTERN: &str = r"event \d+";

/// A server over an empty data root in `work`, with the production
/// subscriber installed and 20 events for service `fresh` in its hot
/// buffer, whose queries time out after `timeout_secs`.
async fn regex_server(work: &Path, timeout_secs: u64) -> common::TestServer {
    regex_server_with(work, timeout_secs, common::QueryKnobs::DEFAULT).await
}

/// [`regex_server`] with the query pool and budgets `knobs`.
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

/// A live stream of `dsl`, answered with a status: for a stream that
/// setup refuses.
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

/// ` | where message matches /…/` for each body.
fn matches_stages(bodies: &[&str]) -> String {
    bodies.iter().fold(String::new(), |mut stages, body| {
        stages.push_str(" | where message matches /");
        stages.push_str(body);
        stages.push('/');
        stages
    })
}

/// A query whose metered preparation needs more regex memory than one
/// query may hold answers 503 `query_resource_exhausted` with the fixed
/// sentence, and its `http_failure` names the regex budget as its cause
/// (ADR-0063). Stream setup refuses it over the live filter and over a
/// stage, and the batch lane over the Rust tail behind an `extract kv`.
/// Nothing the server answers or records quotes a pattern, and an
/// ordinary query answers afterwards.
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
                !record.contains("w{"),
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

/// The server-wide regex reservation follows the physical work, not the
/// request (ADR-0063). The child lowers the reservation to one tail's
/// charge and a build transient, less a byte, so while one stream or tail
/// holds its charge no other build can start.
///
/// The child reads the reservation and its hold and release counters
/// directly: the server runs in the child's own process, as the pool's
/// seams the other tests here hold do.
///
/// - An open stream holds its charge, and returns it when it closes.
/// - A request that cannot reserve, from stream setup or the batch tail,
///   answers 503 with cause `regex_reservation_exhausted`.
/// - A batch tail paused after its charge outlives its request's deadline:
///   the client gets 504, the charge stays held, and another tail is
///   refused, until the work exits and returns every byte.
#[test]
fn regex_reservation_follows_physical_work() {
    let work = tempfile::tempdir().expect("work dir");
    run_child("regex_reservation", work.path(), &[]);
}

/// The child half of [`regex_reservation_follows_physical_work`].
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

    // An open stream holds its charge.
    let stream = open_stream(&server, &live).await;
    assert_eq!(stream.status(), 200);
    assert_eq!(
        reservation.reserved(),
        charge,
        "the open stream holds its charge"
    );

    // While it does, no other build can reserve its transient.
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

    // Closing the stream returns its charge. Events on the bus make the
    // stream write, so the server finds the connection closed even if it
    // never read the close.
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

    // A batch tail paused once its charge is held, past its deadline.
    let seams = server.state.query.pool.seams();
    let released = seams.watch(Seam::Released);
    let pause = reservation.pause_after_charge();
    tokio::time::pause();
    let mut request = std::pin::pin!(post_query(&server, &tail));
    // Waits for: the tail's arrival at the gate, its charge held.
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
    // Waits for: the request's answer, with the work still at the gate.
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

    // The charge is the work's: another tail cannot reserve while it runs.
    let mut second = std::pin::pin!(post_query(&server, &tail));
    // Waits for: the second tail's answer, refused at its reservation.
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
    // Waits for: the timed-out work's arrival at Released.
    until("the timed-out work ends", || released.arrivals() > ended).await;
    assert_eq!(reservation.reserved(), 0, "the work returned its charge");
    assert_eq!(reservation.holds(), reservation.releases());
    drop(pause);

    // With the bytes back, the same tail answers.
    let mut again = std::pin::pin!(post_query(&server, &tail));
    // Waits for: the tail's answer.
    let answer = until_polling("the tail answers", &mut again, || false)
        .await
        .expect("until_polling returns only once the request answers");
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(reservation.reserved(), 0);
    assert_eq!(reservation.holds(), reservation.releases());
}

/// A batch query of `n` regex sites: `n` `matches` stages, each a
/// different pattern, so each binds one regex of its own.
fn sites_query(n: usize) -> String {
    let bodies: Vec<String> = (0..n).map(|i| format!(r"event {i}\d*")).collect();
    let bodies: Vec<&str> = bodies.iter().map(String::as_str).collect();
    format!("service=fresh{}", matches_stages(&bodies))
}

/// The RE2 allowance of a statement of `n` `matches` sites on `threads`.
fn matching_allowance(n: u64, threads: NonZeroU32) -> u64 {
    memory::RegexSites {
        extract: 0,
        matching: n,
        fixed: 0,
    }
    .re2_allowance(threads)
}

/// A batch statement holds its RE2 allowance from the server-wide regex
/// reservation, every regex site `DuckDB` binds times the query threads,
/// taken before the statement binds and held for the physical work
/// (ADR-0063).
///
/// The child runs at the shipped reservation, 1.25 GiB, and reads the
/// reservation and its counters directly, as
/// [`regex_reservation_follows_physical_work`]'s child does.
///
/// - A query with no regex site takes no hold.
/// - A batch query paused once its allowance is held holds exactly that
///   allowance, before its statement binds and past its request's
///   deadline, until its work exits, and then returns every byte.
/// - A query whose allowance passes the per-query cap can never run: 503,
///   cause `regex_budget_exhausted`, and nothing held.
/// - With the reservation filled by two held queries just under the cap,
///   a third answers 503, cause `regex_reservation_exhausted`.
#[test]
fn regex_reservation_counts_batch_re2() {
    let work = tempfile::tempdir().expect("work dir");
    run_child("regex_batch_re2", work.path(), &[]);
}

/// The child half of [`regex_reservation_counts_batch_re2`].
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

    // No regex site, no hold.
    let answer = post_query(&server, "service=fresh | stats count()").await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!((reservation.holds(), reservation.reserved()), (0, 0));

    // One site, held at the gate past the request's deadline.
    let one = matching_allowance(1, threads);
    let seams = server.state.query.pool.seams();
    let released = seams.watch(Seam::Released);
    let pause = reservation.pause_after_charge();
    tokio::time::pause();
    let dsl = sites_query(1);
    let binds = prepare_probe::count();
    let mut request = std::pin::pin!(post_query(&server, &dsl));
    // Waits for: the statement's arrival at the gate, its allowance held.
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
    // Waits for: the request's answer, with the work still at the gate.
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
    // Waits for: the timed-out work's arrival at Released.
    until("the timed-out work ends", || released.arrivals() > ended).await;
    assert_eq!(reservation.reserved(), 0, "the work returned its allowance");
    assert_eq!(reservation.holds(), reservation.releases());
    drop(pause);

    // Past the per-query cap: refused before anything is held.
    let per_cap = memory::QUERY_RESERVATION_CAP / matching_allowance(1, threads);
    assert!(matching_allowance(per_cap + 1, threads) > memory::QUERY_RESERVATION_CAP);
    let holds = reservation.holds();
    let refused = post_query(&server, &sites_query(usize::try_from(per_cap).unwrap() + 1)).await;
    assert_exhausted(&refused);
    assert_exhausted_cause(QUERY_ROUTE, &refused.request_id, "regex_budget_exhausted");
    assert_eq!((reservation.holds(), reservation.reserved()), (holds, 0));

    // Two queries just under the cap fill the reservation: a third that
    // would fit the cap alone does not fit beside them.
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
    // Waits for: both statements at the gate, their allowances held.
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
    // Waits for: the third query's answer, refused at its reservation.
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
    // Waits for: both held queries' answers.
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
    // Waits for: the work behind both answers to return its allowance.
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

/// The fixed regex calls [`FEW_SEV`] binds.
const FEW_SEV_FIXED: u64 = 123;

/// The regexes the emitter writes itself, the four `sev()` and a
/// `SEVERITY` conform carry, take their RE2 allowance beside the query's
/// own patterns: each call `DuckDB` binds, at
/// [`memory::FIXED_SITE_RE2_BYTES`], once on each query thread (ADR-0063).
///
/// The child runs at the shipped reservation and the default four query
/// threads, and reads the reservation directly, as
/// [`regex_reservation_counts_batch_re2`]'s child does.
///
/// - An ordinary query of three `sev()` calls holds the allowance of
///   their 123 fixed calls while its statement is paused, then answers
///   200 and returns every byte.
/// - A query of 1000 `sev()` calls, 41 000 fixed calls, about 1.4 GiB on
///   four threads, passes the per-query cap and can never run: 503, cause
///   `regex_budget_exhausted`, and nothing held.
#[test]
fn fixed_regex_sites_take_their_allowance() {
    let work = tempfile::tempdir().expect("work dir");
    run_child("regex_fixed_sites", work.path(), &[]);
}

/// The child half of [`fixed_regex_sites_take_their_allowance`].
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

    // A few sev() calls: admitted, holding their allowance.
    let few = fixed(FEW_SEV_FIXED);
    assert!(few <= memory::QUERY_RESERVATION_CAP);
    let pause = reservation.pause_after_charge();
    let mut request = std::pin::pin!(post_query(&server, FEW_SEV));
    // Waits for: the statement's arrival at the gate, its allowance held.
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
    // Waits for: the query's answer.
    let answer = until_polling("the query answers", &mut request, || false)
        .await
        .expect("until_polling returns only once the request answers");
    assert_eq!(answer.status, 200, "{}", answer.body);
    drop(pause);
    // Waits for: the work behind the answer to return its allowance.
    until("the work returns its allowance", || {
        reservation.reserved() == 0
    })
    .await;
    assert_eq!(reservation.holds(), reservation.releases());

    // A thousand: past the per-query cap, refused before anything is held.
    let lets: Vec<String> = (0..1000).map(|i| format!("s{i} = sev(message)")).collect();
    let thousand = format!("service=fresh | let {}", lets.join(", "));
    assert!(fixed(1000 * 41) > memory::QUERY_RESERVATION_CAP);
    let holds = reservation.holds();
    let refused = post_query(&server, &thousand).await;
    assert_exhausted(&refused);
    assert_exhausted_cause(QUERY_ROUTE, &refused.request_id, "regex_budget_exhausted");
    assert_eq!((reservation.holds(), reservation.reserved()), (holds, 0));
}

/// The admission bar of the RE2 allowance (ADR-0063): as many ordinary
/// queries as `max_concurrent_queries`' default runs at most, each a
/// search of five corpus patterns on the default query threads, fit the
/// reservation together, and each fits the per-query cap. So do as many
/// of the same search reading a `sev()` and filtering on it.
#[test]
fn ordinary_queries_fit_the_reservation_together() {
    let five: Vec<String> = charge::LOG_REGEXES
        .iter()
        .take(5)
        .map(|body| format!("/{body}/"))
        .collect();
    let search = format!("message={}", five.join(","));
    let with_sev = format!("{search} | let s = sev(message) | where s >= 9");
    for (dsl, fixed) in [(&search, 0), (&with_sev, 82)] {
        let query = trawl_core::parser::parse(dsl).expect("the ordinary query parses");
        let emitted = trawl_core::emitter::emit(
            &query,
            "/data/*.parquet",
            trawl_core::context::EvalContext::capture(),
        )
        .expect("the ordinary query emits");
        assert_eq!(
            emitted.regex_sites,
            memory::RegexSites {
                extract: 0,
                matching: 5,
                fixed,
            },
            "{dsl}"
        );
        let one = emitted
            .regex_sites
            .re2_allowance(trawl_server::config::DEFAULT_QUERY_THREADS);
        assert!(one <= memory::QUERY_RESERVATION_CAP, "{dsl}");
        let queries = u64::try_from(trawl_server::config::MAX_DEFAULT_CONCURRENT_QUERIES).unwrap();
        assert!(
            queries * one <= memory::REGEX_RESERVATION_BYTES,
            "{queries} ordinary queries hold {} bytes, over the reservation of {}: {dsl}",
            queries * one,
            memory::REGEX_RESERVATION_BYTES
        );
    }
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
