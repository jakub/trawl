// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The pool holds what a request's schema probe needs (ADR-0062).
//!
//! The probe describes the request's cold reader once, before the
//! statement it shapes binds. It runs inside the pool's slot: under the
//! request's permit, executor and deadline, and under the publication read
//! guard the slot takes before source selection and releases only when the
//! work ends. These are the pool twins of the engine tests of the same
//! names in `trawl-engine/tests/schema_probe.rs`, which pin the lane's half:
//! the engine has no permit, no deadline and no publication guard of its
//! own.
//!
//! Both tests park the probe inside `DuckDB` with the engine's test hook
//! ([`schema_probe::park_when`]), keyed by the corpus directory so no other
//! test's probe parks.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value as Json, json};
use tokio::time::Instant;
use trawl_engine::executor::schema_probe;
use trawl_engine::value::{QueryResult, Value};
use trawl_server::bus::IngestBatch;
use trawl_server::deadline::Deadline;
use trawl_server::error::ServerError;
use trawl_server::hot_buffer::{HotBuffer, HotBufferConfig};
use trawl_server::ingest::compaction::compact_once;
use trawl_server::ingest::wal::WalWriter;
use trawl_server::pool::{ExecutorPool, WorkContext, WorkKind, seam::Seam};
use trawl_server::test_timing::HANG_GUARD;

/// The value of `f` on every event the late batch carries.
const LATE_F: &str = "late";

/// One `nginx` event; `f` only when given.
fn event(message: &str, f: Option<&str>) -> Map<String, Json> {
    let mut event = json!({
        "_time": "2026-02-15T12:00:00Z",
        "_ingested": "2026-02-15T12:00:01Z",
        "service": "nginx",
        "_severity": 9,
        "message": message,
    })
    .as_object()
    .expect("an object")
    .clone();
    if let Some(f) = f {
        event.insert("f".to_owned(), json!(f));
    }
    event
}

/// A WAL, a corpus directory and a hot buffer, as the server lays them out.
struct Corpus {
    _root: tempfile::TempDir,
    wal: std::path::PathBuf,
    data: std::path::PathBuf,
    hot: Arc<HotBuffer>,
}

impl Corpus {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("corpus root");
        let wal = root.path().join("wal");
        let data = root.path().join("data");
        std::fs::create_dir_all(&data).expect("corpus directory");
        Self {
            _root: root,
            wal,
            data,
            hot: Arc::new(HotBuffer::new(HotBufferConfig {
                max_events: 1_000,
                max_bytes: 1_000_000,
            })),
        }
    }

    fn data_dir(&self) -> String {
        self.data.to_str().expect("utf-8 corpus path").to_owned()
    }

    /// Accept `events` as the ingest handler does: a WAL file and the same
    /// batch in the hot buffer.
    fn accept(&self, events: Vec<Map<String, Json>>) {
        let mut ndjson = Vec::new();
        for event in &events {
            serde_json::to_writer(&mut ndjson, event).expect("an event");
            ndjson.push(b'\n');
        }
        let writer = WalWriter::new(self.wal.clone());
        writer.ensure_dir().expect("WAL directory");
        let path = writer.write("prod", "nginx", &ndjson).expect("WAL write");
        self.hot.insert_for_test(Arc::new(IngestBatch {
            batch_id: format!("prod/{}", path.file_stem().unwrap().to_str().unwrap()).into(),
            service: "nginx".into(),
            byte_size: ndjson.len(),
            events,
        }));
    }

    /// One compaction pass: the WAL into cold parquet, the hot batches
    /// drained under the publication write guard.
    async fn compact(wal: std::path::PathBuf, data: std::path::PathBuf, hot: Arc<HotBuffer>) {
        let errors = compact_once(
            &wal,
            &data,
            Duration::ZERO,
            false,
            Some(&hot),
            500,
            "2GB",
            None,
        )
        .await
        .expect("compaction runs");
        assert_eq!(errors, 0, "compaction publishes every batch");
    }

    /// Compact what was accepted, now.
    async fn compact_now(&self) {
        Self::compact(self.wal.clone(), self.data.clone(), Arc::clone(&self.hot)).await;
    }

    /// A publication marker in the WAL: a compaction has staged its
    /// output and is about to take the publication write guard.
    fn publishing(&self) -> bool {
        std::fs::read_dir(self.wal.join("prod")).is_ok_and(|entries| {
            entries.flatten().any(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(".publish-"))
            })
        })
    }
}

/// The next probe over a corpus, parked inside `DuckDB` until released.
struct Park {
    parked: mpsc::Receiver<Instant>,
    release: mpsc::Sender<()>,
    _hook: schema_probe::FaultGuard,
}

impl Park {
    /// Park every probe whose `DESCRIBE` names `corpus`. The hook reports
    /// when the probe parked.
    fn probes_over(corpus: &str) -> Self {
        let (parked_tx, parked) = mpsc::channel();
        let (release, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let hook = schema_probe::park_when(corpus, move || {
            parked_tx
                .send(Instant::now())
                .expect("the test waits for the park");
            release_rx
                .lock()
                .expect("unpoisoned")
                .recv_timeout(HANG_GUARD)
                .expect("one park per request, and the test releases it");
        });
        Self {
            parked,
            release,
            _hook: hook,
        }
    }

    /// The instant the probe parked. The bound only names a hang.
    fn wait(&self) -> Instant {
        tokio::task::block_in_place(|| {
            self.parked
                .recv_timeout(HANG_GUARD)
                .expect("the request's probe parks")
        })
    }

    fn release(&self) {
        self.release.send(()).expect("the probe waits");
    }
}

/// Poll `ready` until it holds. The bound only names a hang.
async fn until(what: &str, ready: impl Fn() -> bool) {
    tokio::time::timeout(HANG_GUARD, async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}: never happened"));
}

/// `(message, f)` for every row of an answer that projects both.
fn message_and_f(result: &QueryResult) -> Vec<(String, Option<String>)> {
    let column = |name: &str| {
        result
            .columns
            .iter()
            .position(|column| column.name == name)
            .unwrap_or_else(|| panic!("no {name} column in {:?}", result.columns))
    };
    let (message_at, f_at) = (column("message"), column("f"));
    let text = |value: &Value| match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        other => panic!("a text cell, got {other:?}"),
    };
    result
        .rows
        .iter()
        .map(|row| {
            (
                text(&row[message_at]).expect("every event has a message"),
                text(&row[f_at]),
            )
        })
        .collect()
}

/// The late batch's rows in an answer, by their `f`.
fn late_rows(rows: &[(String, Option<String>)]) -> Vec<Option<&str>> {
    rows.iter()
        .filter(|(message, _)| message.starts_with("late"))
        .map(|(_, f)| f.as_deref())
        .collect()
}

/// A probe parked past the request's deadline releases nothing early. The
/// request answers 504 on the budget it was stamped with, while the probe
/// is still parked: the probe runs on that budget and never refreshes it.
/// The work keeps its permit, its executor and its publication guard until
/// it stops, and gives them all back once the probe is released and the
/// lane, cancelled at the deadline, returns.
///
/// The worker is held before source selection for half the budget, so the
/// probe parks late: a deadline refreshed when the probe began could not
/// have run out before `parked_at + BUDGET`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parked_probe_keeps_its_permit() {
    const BUDGET: Duration = Duration::from_millis(400);
    const CAPACITY: usize = 2;
    let corpus = Corpus::new();
    corpus.accept(vec![event("early", None)]);
    corpus.compact_now().await;
    let pool = ExecutorPool::for_test(
        corpus.data_dir(),
        CAPACITY,
        100,
        Some(Arc::clone(&corpus.hot)),
    )
    .expect("pool");

    let park = Park::probes_over(&corpus.data_dir());
    let seams = pool.seams();
    let started = seams.hold(Seam::Started);
    let released = seams.watch(Seam::Released);
    let deadline = Deadline::after(BUDGET);
    let request = {
        let pool = pool.clone();
        let id = pool.allocate_query_id();
        tokio::spawn(async move {
            pool.execute(
                id,
                "service=nginx",
                deadline,
                false,
                0,
                WorkContext::system(WorkKind::Query),
            )
            .await
        })
    };
    until("the worker starts", || started.arrivals() == 1).await;
    tokio::time::sleep(BUDGET / 2).await;
    started.release();
    let parked_at = park.wait();

    let answer = tokio::time::timeout(HANG_GUARD, request)
        .await
        .expect("the request answers while its probe is parked")
        .expect("the request task");
    let answered_at = Instant::now();
    assert!(
        matches!(answer.result, Err(ServerError::Timeout)),
        "a started query past its deadline is a timeout: {:?}",
        answer.result
    );
    assert!(
        answered_at >= deadline.instant(),
        "the request waited out its budget"
    );
    assert!(
        answered_at < parked_at + BUDGET,
        "the probe ran on the request's budget, not a fresh one: answered {:?} after the park",
        answered_at - parked_at
    );

    // The probe is still parked, so nothing has been given back.
    assert_eq!(released.arrivals(), 0, "the work has not ended");
    assert_eq!(
        pool.available_permits(),
        CAPACITY - 1,
        "the parked work keeps its permit"
    );
    assert_eq!(pool.retained(), 1, "the parked work is retained");
    let retained = pool.retained_work();
    assert_eq!(retained.len(), 1, "{retained:?}");
    assert!(
        retained[0].started,
        "the retained work holds its executor: {retained:?}"
    );
    let publication = pool.publication();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), publication.write())
            .await
            .is_err(),
        "the parked work keeps its publication guard"
    );

    park.release();
    until("the work releases everything", || released.arrivals() == 1).await;
    assert_eq!(pool.available_permits(), CAPACITY, "the permit is back");
    assert_eq!(pool.retained(), 0, "nothing is retained");
    tokio::time::timeout(HANG_GUARD, publication.write())
        .await
        .expect("the publication guard is back");
}

/// Compaction's publish waits on the publication write guard while a
/// request holding the read guard is parked in its probe. After the
/// request ends, the publish lands; the request's answer read the late
/// batch from the hot buffer, with its real `f`, and excluded the file
/// that later carried it. The next request reads that file's real `f`.
///
/// Without the guard the probe could describe the cold files before the
/// file arrived and the statement read them after: the late rows would
/// then come back twice, once from the hot snapshot and once from the
/// file, the file's copy with `f` as NULL.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)] // one controlled publication, checked from both sides
async fn probe_respects_the_publication_guard() {
    const DSL: &str = "* | table message, f";
    let corpus = Corpus::new();
    corpus.accept(vec![event("early one", None), event("early two", None)]);
    corpus.compact_now().await;
    assert_eq!(corpus.hot.event_count(), 0, "the early batch is cold");
    corpus.accept(vec![
        event("late one", Some(LATE_F)),
        event("late two", Some(LATE_F)),
        event("late three", Some(LATE_F)),
    ]);
    let pool = ExecutorPool::for_test(corpus.data_dir(), 2, 100, Some(Arc::clone(&corpus.hot)))
        .expect("pool");

    let park = Park::probes_over(&corpus.data_dir());
    let request = {
        let pool = pool.clone();
        let id = pool.allocate_query_id();
        tokio::spawn(async move {
            pool.execute(
                id,
                DSL,
                Deadline::after(HANG_GUARD),
                false,
                0,
                WorkContext::system(WorkKind::Query),
            )
            .await
        })
    };
    park.wait();

    let (published, release_publication) =
        corpus.hot.publication().pause_next_publication_for_test();
    let compaction = tokio::spawn(Corpus::compact(
        corpus.wal.clone(),
        corpus.data.clone(),
        Arc::clone(&corpus.hot),
    ));
    // The marker is written durably just before the write guard is
    // taken, so from here the compaction is at, or about to block on,
    // the guard the parked request holds.
    until("the compaction stages its output", || corpus.publishing()).await;
    assert!(
        tokio::task::block_in_place(|| published.recv_timeout(Duration::from_millis(100))).is_err(),
        "the publish waits for the request parked in its probe"
    );
    assert_eq!(
        corpus.hot.event_count(),
        3,
        "nothing published, nothing drained"
    );

    park.release();
    let answer = tokio::time::timeout(HANG_GUARD, request)
        .await
        .expect("the request answers once its probe is released")
        .expect("the request task")
        .result
        .expect("the request succeeds");
    // The publish lands after the request ends.
    tokio::task::block_in_place(|| published.recv_timeout(HANG_GUARD))
        .expect("the publish lands once the request released its guard");
    release_publication
        .send(())
        .expect("the compaction waits at its publication");
    tokio::time::timeout(HANG_GUARD, compaction)
        .await
        .expect("the compaction finishes")
        .expect("the compaction task");
    assert_eq!(corpus.hot.event_count(), 0, "the late batch is cold now");

    let rows = message_and_f(&answer);
    assert_eq!(
        late_rows(&rows),
        [Some(LATE_F); 3],
        "the request read the late batch once, with its real f: {rows:?}"
    );
    assert_eq!(
        rows.iter()
            .filter(|(message, _)| message.starts_with("early"))
            .count(),
        2,
        "the request read the cold file: {rows:?}"
    );

    drop(park);
    let after = pool
        .execute(
            pool.allocate_query_id(),
            DSL,
            Deadline::after(HANG_GUARD),
            false,
            0,
            WorkContext::system(WorkKind::Query),
        )
        .await
        .result
        .expect("the next request succeeds");
    let rows = message_and_f(&after);
    assert_eq!(
        late_rows(&rows),
        [Some(LATE_F); 3],
        "the next request reads the published file's real f: {rows:?}"
    );
}
