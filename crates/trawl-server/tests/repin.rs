// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod common;

use std::os::unix::fs::MetadataExt as _;
use std::time::Duration;

use common::{DEFAULT_TEST_TIMEOUT_SECS, TestServer, setup_in_dir_with_data_and_timeout};
use serde_json::json;
use trawl_client::{HttpClient, RepinCeilings, RepinStart};
use trawl_server::catalog::CatalogContext;
use trawl_server::config::RateLimitConfig;
use trawl_server::deadline::Deadline;
use trawl_server::pool::{WorkContext, WorkKind};
use trawl_server::repin::ceiling::RequestedCeilings;
use trawl_server::repin::{RepinEngine, StartOutcome};
use trawl_server::test_timing::{HANG_GUARD, until_polling};

fn now_ts() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

async fn scrape_metrics(url: &str) -> String {
    common::harness_client_builder()
        .build()
        .unwrap()
        .get(format!("{url}/metrics"))
        .send()
        .await
        .expect("metrics scrape")
        .text()
        .await
        .unwrap()
}

fn gauge_value(scrape: &str, name: &str) -> Option<f64> {
    scrape.lines().find_map(|line| {
        let rest = line.strip_prefix(name)?.strip_prefix(' ')?;
        rest.trim().parse().ok()
    })
}

fn catalog_ctx(server: &TestServer) -> CatalogContext {
    CatalogContext {
        store: server.state.storage.catalog.clone(),
        cache: server.state.query.field_catalog.clone(),
    }
}

struct Harness {
    server: TestServer,
    wal_dir: std::path::PathBuf,
    data_dir: std::path::PathBuf,
    ingest: HttpClient,
    query: HttpClient,
    schema_admin: HttpClient,
}

async fn harness() -> Harness {
    harness_with_query_timeout(DEFAULT_TEST_TIMEOUT_SECS).await
}

/// `[server] timeout_secs` is the hang guard, so a loaded machine makes
/// the answer later instead of turning it into a timeout.
async fn hang_guard_harness() -> Harness {
    harness_with_query_timeout(HANG_GUARD.as_secs()).await
}

/// `timeout_secs` also bounds how long the repin trigger holds its response.
///
/// The rate limit is off: these tests poll in loops for as long as a job
/// takes, and a per-minute budget would fail a correct build with 429.
async fn harness_with_query_timeout(timeout_secs: u64) -> Harness {
    let rate_limit = RateLimitConfig {
        default_rpm: 0,
        ..RateLimitConfig::default()
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let wal_dir = root.join("wal");
    let data_dir = root.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let data_path = data_dir.to_str().unwrap().to_owned();

    let server =
        setup_in_dir_with_data_and_timeout(&root, data_path, rate_limit, timeout_secs).await;
    std::mem::forget(tmp);

    let ingest = HttpClient::new_insecure(&server.url, &server.ingest_token).unwrap();
    let query = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let schema_admin = HttpClient::new_insecure(&server.url, &server.schema_admin_token).unwrap();
    Harness {
        server,
        wal_dir,
        data_dir,
        ingest,
        query,
        schema_admin,
    }
}

impl Harness {
    async fn compact_tick(&self) {
        let hot_buffer = self
            .server
            .state
            .query
            .hot_buffer
            .as_ref()
            .expect("ingest-enabled server has a hot buffer");
        let ctx = catalog_ctx(&self.server);
        let coordinator = self
            .server
            .state
            .ingest
            .repin_coordinator
            .as_ref()
            .expect("ingest-enabled server has a repin coordinator");
        let errors = trawl_server::ingest::compaction::compact_once_coordinated(
            &self.wal_dir,
            &self.data_dir,
            Duration::ZERO,
            Duration::ZERO,
            false,
            Some(hot_buffer),
            500,
            "2GB",
            Some(&ctx),
            Some(coordinator),
        )
        .await
        .expect("compaction tick must succeed");
        assert_eq!(errors, 0, "no compaction errors expected");
    }

    async fn ingest_and_compact(&self, events: &[serde_json::Value]) {
        let resp = self.ingest.ingest(events).await.expect("ingest");
        assert_eq!(resp.accepted, events.len());
        self.compact_tick().await;
    }

    async fn count(&self, dsl: &str) -> i64 {
        let result = self
            .query
            .query_paginated(dsl, None, None)
            .await
            .unwrap_or_else(|e| panic!("query {dsl:?} failed: {e}"));
        if result.result.rows.is_empty() {
            return 0;
        }
        match &result.result.rows[0][0] {
            trawl_engine::value::Value::Integer(n) => *n,
            other => panic!("expected a count, got {other:?}"),
        }
    }

    async fn wait_terminal(&self, id: i64) -> trawl_client::RepinJobResponse {
        let start = std::time::Instant::now();
        while start.elapsed() < HANG_GUARD {
            let status = self
                .schema_admin
                .schema_repin_status()
                .await
                .expect("status");
            if let Some(job) = status.job
                && job.id == id
                && job.status != "running"
            {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("repin job {id} did not reach a terminal state within {HANG_GUARD:?}");
    }

    /// `wait_terminal` is not a barrier for filesystem cleanup: on the success
    /// path the row terminalizes while the task is still sweeping staging, and
    /// the marker is removed last. The task holds the repin permit a little
    /// longer, so [`Harness::wait_idle`] is the barrier before the next job.
    async fn wait_cleanup(&self) {
        let start = std::time::Instant::now();
        while start.elapsed() < HANG_GUARD {
            if !trawl_server::repin::marker_path(&self.data_dir).exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("repin marker still present after {HANG_GUARD:?}");
    }

    /// The permit outlives the terminal row, the marker's removal and the
    /// job generation's end: a started job's task gives it up last.
    /// A yield loop, never a tokio sleep, so it leaves a paused clock where it is.
    async fn wait_idle(&self) {
        let engine = self.engine();
        trawl_server::test_timing::until("the repin permit is released", || engine.repin_idle())
            .await;
    }

    /// Read off the TTL-cached column listing; [`Harness::pinned_type`] reads
    /// `/schema/fields`, which has no cache in front of it.
    async fn schema_endpoint_type(&self, field: &str) -> String {
        let resp = self.query.schema().await.expect("schema");
        resp.columns
            .iter()
            .find(|c| c.name == field)
            .unwrap_or_else(|| panic!("{field} not in /schema columns"))
            .data_type
            .clone()
    }

    async fn pinned_type(&self, field: &str) -> String {
        let resp = self
            .query
            .catalog_fields(None, None, None)
            .await
            .expect("catalog fields");
        resp.fields
            .iter()
            .find(|f| f.name == field)
            .unwrap_or_else(|| panic!("{field} not in catalog"))
            .data_type
            .clone()
    }
}

/// The requester every engine-level call here claims as: key row 1, named
/// `op`. The row id is what the status read by id matches a write-only key
/// against; the name is what the job row and the audit events show.
fn op() -> trawl_server::store::Requester {
    trawl_server::store::Requester::new(1, "op")
}

fn event(service: &str, extra: &serde_json::Value) -> serde_json::Value {
    let mut base = json!({
        "service": service, "env": "prod", "host": "web01",
        "timestamp": now_ts(), "message": "m",
    });
    base.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    base
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // one incomplete rollup, its recovery, and both repin paths
async fn pending_rollup_blocks_repin_before_scan_and_preserves_recovery() {
    use trawl_server::error::ServerError;
    use trawl_server::publication::CorpusUnsettled;
    use trawl_server::store::RepinJobStatus;

    let h = harness().await;
    h.ingest_and_compact(&[
        event("late", &json!({"dur": 1})),
        event("late", &json!({"dur": 2})),
    ])
    .await;
    let source = walk(&h.data_dir)
        .into_iter()
        .find(|path| path.file_name().is_some_and(|name| name == "late.parquet"))
        .unwrap();
    let day = h.data_dir.join("prod/2026-01-01");
    let canonical = day.join("late.parquet");
    let hourly = day.join("01/late.parquet");
    let tmp = day.join("late.parquet.tmp");
    std::fs::create_dir_all(hourly.parent().unwrap()).unwrap();
    let paths = [canonical.clone(), hourly.clone(), tmp.clone()];
    tokio::task::spawn_blocking(move || {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let source_sql = source.to_string_lossy().replace('\'', "''");
        for (path, filter) in paths.iter().zip(["WHERE dur=1", "WHERE dur=2", ""]) {
            conn.execute_batch(&format!(
                "COPY (SELECT * FROM read_parquet('{source_sql}') {filter}) TO '{}' (FORMAT PARQUET)",
                path.to_string_lossy().replace('\'', "''")
            ))
            .unwrap();
        }
        std::fs::remove_file(source).unwrap();
    })
    .await
    .unwrap();

    // The rollup stopped before its rename (case b): the tmp carries the
    // replacement, the daily file the prior merged into it, and the hourly
    // input is still in place, each at its recorded identity.
    let identity = |path: &std::path::Path| {
        trawl_server::ingest::publication_marker::identity_of(path).unwrap()
    };
    let body = trawl_server::ingest::rollup_marker::RollupMarker::new(
        "late",
        identity(&tmp),
        Some(identity(&canonical)),
        vec![("01/late.parquet".to_owned(), identity(&hourly))],
    )
    .unwrap()
    .encode();
    let marker = day.join(".rollup-late");
    let publication = h
        .server
        .state
        .query
        .hot_buffer
        .as_ref()
        .unwrap()
        .publication();
    {
        let _writer = publication.write().await;
        std::fs::write(&marker, &body).unwrap();
        drop(publication.mark_rollup(&marker));
    }
    let originals: Vec<_> = [&canonical, &hourly, &tmp, &marker]
        .into_iter()
        .map(|path| (path.clone(), std::fs::read(path).unwrap()))
        .collect();
    let engine = h.engine();
    let coordinator = h.server.state.ingest.repin_coordinator.as_ref().unwrap();
    match h
        .schema_admin
        .schema_repin(
            "dur",
            "VARCHAR",
            None,
            true,
            false,
            RepinCeilings::default(),
        )
        .await
    {
        Err(trawl_client::ClientError::Server { status, error, .. }) => {
            assert_eq!(status, 503);
            assert_eq!(error.code, trawl_api::ErrorCode::CorpusRecovering);
            assert_eq!(
                error.message,
                "Search is unavailable while the server finishes an interrupted storage rollup."
            );
        }
        other => panic!("a pending rollup must refuse the repin request: {other:?}"),
    }
    for dry_run in [true, false] {
        assert!(matches!(
            engine
                .start(
                    "dur",
                    "VARCHAR",
                    None,
                    dry_run,
                    false,
                    RequestedCeilings::default(),
                    Some(&op())
                )
                .await,
            Err(ServerError::CorpusRecovering(
                CorpusUnsettled::RollupPending
            ))
        ));
        let job = engine.store().latest().await.unwrap().unwrap();
        assert_eq!(job.status, RepinJobStatus::Blocked);
        assert_eq!(
            job.error.as_deref(),
            Some("repin is blocked while rollup publication is incomplete; retry after recovery"),
            "the job row keeps the reason-specific text"
        );
        assert!(
            job.planned_at.is_none(),
            "no scan report may describe a pending rollup"
        );
        assert_eq!(job.files_done, 0);
        assert!(!coordinator.rollup_paused());
        assert!(!trawl_server::repin::marker_path(&h.data_dir).exists());
        assert!(!trawl_server::repin::shadow_root(&h.data_dir).exists());
        assert!(!trawl_server::repin::aside_root(&h.data_dir).exists());
        for (path, bytes) in &originals {
            assert_eq!(
                &std::fs::read(path).unwrap(),
                bytes,
                "{} changed",
                path.display()
            );
        }
    }

    h.compact_tick().await;
    assert!(!marker.exists());
    assert!(!tmp.exists());
    assert!(!hourly.exists());
    assert_eq!(h.count("service=late | stats count()").await, 2);
    assert_eq!(h.count("service=late dur=2 | stats count()").await, 1);

    let dry = engine
        .start(
            "dur",
            "VARCHAR",
            None,
            true,
            false,
            RequestedCeilings::default(),
            Some(&op()),
        )
        .await
        .unwrap();
    assert!(matches!(dry, StartOutcome::DryRun(_)));
    assert_eq!(
        engine.store().latest().await.unwrap().unwrap().status,
        RepinJobStatus::Succeeded
    );
    assert!(
        !coordinator.rollup_paused(),
        "the dry run releases its admission pause"
    );
    let started = match engine
        .start(
            "dur",
            "VARCHAR",
            None,
            false,
            false,
            RequestedCeilings::default(),
            Some(&op()),
        )
        .await
        .unwrap()
    {
        StartOutcome::Started(job) => job,
        other => panic!("expected a started repin after recovery, got {other:?}"),
    };
    assert_eq!(h.wait_terminal(started.id).await.status, "succeeded");
    h.wait_cleanup().await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while coordinator.rollup_paused() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the completed job releases its admission pause");
    assert_eq!(h.count("service=late | stats count()").await, 2);
}

#[tokio::test]
async fn cancelling_repin_during_rollup_admission_terminalizes_the_job() {
    use trawl_server::repin::cancel::{CancelActor, CancelVerdict};
    use trawl_server::store::RepinJobStatus;

    let h = harness().await;
    h.ingest_and_compact(&[event("api", &json!({"dur": 1}))])
        .await;
    let engine = h.engine();
    let coordinator = h.server.state.ingest.repin_coordinator.as_ref().unwrap();
    let active = coordinator.rollup_unit_guard().await.unwrap();
    let request_engine = engine.clone();
    let request = tokio::spawn(async move {
        request_engine
            .start(
                "dur",
                "VARCHAR",
                None,
                false,
                false,
                RequestedCeilings::default(),
                Some(&op()),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while engine.store().latest().await.unwrap().is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the request claims a job before waiting on the rollup");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match engine.cancel(&CancelActor::new("operator", "test-key")) {
                CancelVerdict::Cancelling { .. } => break,
                // The committed claim can precede the in-process cancel
                // registry by the response from that database write.
                CancelVerdict::NoJobRunning => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                CancelVerdict::PastPointOfNoReturn { .. } => {
                    panic!("a job waiting for admission cannot reach cutover");
                }
            }
        }
    })
    .await
    .expect("the waiting job arms its cancellation registry");
    let outcome = tokio::time::timeout(Duration::from_secs(5), request)
        .await
        .expect("cancellation cannot wait for the active rollup")
        .unwrap()
        .unwrap();
    assert!(matches!(outcome, StartOutcome::Cancelled(_)));
    assert_eq!(
        engine.store().latest().await.unwrap().unwrap().status,
        RepinJobStatus::Cancelled
    );
    assert!(!coordinator.rollup_paused());
    assert!(!trawl_server::repin::marker_path(&h.data_dir).exists());
    assert!(!trawl_server::repin::shadow_root(&h.data_dir).exists());
    drop(active);
    assert_eq!(h.count("service=api | stats count()").await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn repin_is_invisible_to_queries_and_resurrects_shelved_values() {
    let h = hang_guard_harness().await;

    // First typed sight pins BIGINT; the enum-shaped batch then conflicts
    // and its value is shelved into _raw.
    h.ingest_and_compact(&[
        event("api", &json!({"status": 200})),
        event("api", &json!({"status": 404})),
    ])
    .await;
    h.ingest_and_compact(&[event("api", &json!({"status": "accepted"}))])
        .await;
    h.ingest_and_compact(&[event("quiet", &json!({"other": 1}))])
        .await;
    assert_eq!(h.pinned_type("status").await, "BIGINT");

    let foreign = h.data_dir.join("prod/notes.parquet");
    std::fs::write(&foreign, b"not even parquet, and not trawl's").unwrap();
    let foreign_ino = std::fs::metadata(&foreign).unwrap().ino();

    let quiet_file = {
        let files = walk(&h.data_dir);
        files
            .into_iter()
            .find(|p| p.file_name().is_some_and(|n| n == "quiet.parquet"))
            .expect("quiet.parquet exists")
    };
    let quiet_ino = std::fs::metadata(&quiet_file).unwrap().ino();

    let q_ordered = "status>=400 last=1h | stats count()";
    let q_eq = "last=1h | where status == 200 | stats count()";
    // (`where status == "accepted"` is unaskable under the BIGINT pin —
    // text-vs-numeric equality is a query error, pin-blind and pin-aware
    // alike — so shelving is asserted through the presence filter.)
    let q_accepted = "last=1h | where status == \"accepted\" | stats count()";
    let q_present = "status=* last=1h | stats count()";
    let before_ordered = h.count(q_ordered).await;
    let before_eq = h.count(q_eq).await;
    assert_eq!(before_ordered, 1);
    assert_eq!(before_eq, 1);
    assert_eq!(
        h.count(q_present).await,
        2,
        "the conflict value is shelved: only two rows carry a stored status"
    );

    let dry = match h
        .schema_admin
        .schema_repin(
            "status",
            "varchar",
            None,
            true,
            false,
            RepinCeilings::default(),
        )
        .await
        .expect("dry run")
    {
        RepinStart::Report(job) => job,
        other => panic!("expected a dry-run report, got {other:?}"),
    };
    assert_eq!(dry.status, "succeeded");
    assert_eq!(dry.files_total, 1, "only api.parquet carries the column");
    assert_eq!(dry.rows_carrying, 2);
    assert_eq!(dry.projected_nulls, 0);
    assert_eq!(dry.resurrectable, 1);

    let started = match h
        .schema_admin
        .schema_repin(
            "status",
            "VARCHAR",
            None,
            false,
            false,
            RepinCeilings::default(),
        )
        .await
        .expect("execute")
    {
        RepinStart::Started(job) => job,
        other => panic!("expected started, got {other:?}"),
    };
    let done = h.wait_terminal(started.id).await;
    assert_eq!(done.status, "succeeded", "error: {:?}", done.error);
    assert_eq!(done.rows_resurrected, dry.resurrectable);
    assert_eq!(done.rows_nulled, dry.projected_nulls);
    assert_eq!(done.files_done, dry.files_total);

    assert_eq!(h.pinned_type("status").await, "VARCHAR");

    assert_eq!(h.count(q_ordered).await, before_ordered);
    assert_eq!(h.count(q_eq).await, before_eq);
    assert_eq!(h.count(q_present).await, 3);
    assert_eq!(h.count(q_accepted).await, 1);

    assert_eq!(
        std::fs::metadata(&quiet_file).unwrap().ino(),
        quiet_ino,
        "unaffected files ride as hardlinks"
    );
    assert_eq!(
        std::fs::read(&foreign).unwrap(),
        b"not even parquet, and not trawl's"
    );
    assert_eq!(std::fs::metadata(&foreign).unwrap().ino(), foreign_ino);

    // The success row lands inside the cutover and the sweep follows it, so
    // wait for the job generation to read idle before reading the aftermath.
    h.await_jobs_idle().await;
    assert!(!trawl_server::repin::shadow_root(&h.data_dir).exists());
    assert!(!trawl_server::repin::aside_root(&h.data_dir).exists());
    assert!(!trawl_server::repin::marker_path(&h.data_dir).exists());

    h.ingest_and_compact(&[event("api", &json!({"status": 503}))])
        .await;
    assert_eq!(h.count("status>=400 last=1h | stats count()").await, 2);
}

/// The harness TTL is 60s (`tests/common/mod.rs`) and this test sleeps
/// nowhere, so TTL expiry cannot explain a pass.
#[tokio::test(flavor = "multi_thread")]
async fn a_cutover_retypes_the_schema_endpoint_immediately() {
    let h = harness().await;

    h.ingest_and_compact(&[
        event("api", &json!({"status": 200})),
        event("api", &json!({"status": 404})),
    ])
    .await;

    // Prime the cache slot: this read populates it under the BIGINT pin.
    assert_eq!(h.schema_endpoint_type("status").await, "BIGINT");

    let started = match h
        .schema_admin
        .schema_repin(
            "status",
            "VARCHAR",
            None,
            false,
            false,
            RepinCeilings::default(),
        )
        .await
        .expect("execute")
    {
        RepinStart::Started(job) => job,
        other => panic!("expected started, got {other:?}"),
    };
    let done = h.wait_terminal(started.id).await;
    assert_eq!(done.status, "succeeded", "error: {:?}", done.error);

    assert_eq!(
        h.schema_endpoint_type("status").await,
        "VARCHAR",
        "/schema must retype the moment the cutover flips the pin"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pending_publication_marker_refuses_the_cutover() {
    use trawl_server::ingest::publication_marker::{
        OutputIdentity, ValidatedMarker, remove_marker_durably, write_marker,
    };

    let h = harness().await;
    h.ingest_and_compact(&[
        event("api", &json!({"status": 200})),
        event("api", &json!({"status": 404})),
    ])
    .await;
    let marker = ValidatedMarker::new(
        "prod",
        "api",
        chrono::Utc::now().date_naive(),
        7,
        vec!["api_1730000000000_0001.ndjson".to_owned()],
        OutputIdentity {
            size: 4,
            hash: blake3::hash(b"PAR1"),
        },
    )
    .unwrap();
    write_marker(&h.wal_dir, &marker).unwrap();
    let before = corpus_digest(&h.data_dir);

    let start = || async {
        match h
            .schema_admin
            .schema_repin(
                "status",
                "VARCHAR",
                None,
                false,
                false,
                RepinCeilings::default(),
            )
            .await
            .expect("execute")
        {
            RepinStart::Started(job) => job,
            other => panic!("expected started, got {other:?}"),
        }
    };

    let refused = h.wait_terminal(start().await.id).await;
    assert_eq!(refused.status, "blocked", "error: {:?}", refused.error);
    let error = refused.error.unwrap_or_default();
    assert!(error.contains("publication_marker_pending"), "{error}");
    assert!(
        !error.contains(&h.wal_dir.display().to_string()),
        "no paths on the wire: {error}"
    );
    assert_eq!(h.pinned_type("status").await, "BIGINT");
    assert_eq!(corpus_digest(&h.data_dir), before, "corpus untouched");
    assert!(!trawl_server::repin::shadow_root(&h.data_dir).exists());
    assert!(!trawl_server::repin::aside_root(&h.data_dir).exists());
    assert!(!trawl_server::repin::marker_path(&h.data_dir).exists());

    remove_marker_durably(&marker.marker_path(&h.wal_dir)).unwrap();
    h.wait_idle().await;
    let done = h.wait_terminal(start().await.id).await;
    assert_eq!(done.status, "succeeded", "error: {:?}", done.error);
    assert_eq!(h.pinned_type("status").await, "VARCHAR");
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&d) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    out.push(p);
                }
            }
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn lossy_repin_refuses_without_force_and_accounts_with_it() {
    let h = harness().await;
    let timeouts_before = common::bookkeeping_timeouts(&h.server.url).await;

    // A half-numeric text field pins VARCHAR (the ladder needs >=90%).
    h.ingest_and_compact(&[
        event("api", &json!({"dur": "12"})),
        event("api", &json!({"dur": "oops"})),
    ])
    .await;
    assert_eq!(h.pinned_type("dur").await, "VARCHAR");

    let refused = match h
        .schema_admin
        .schema_repin(
            "dur",
            "BIGINT",
            None,
            false,
            false,
            RepinCeilings::default(),
        )
        .await
        .expect("refusal is a decoded outcome, not a transport error")
    {
        RepinStart::Refused(job) => job,
        other => panic!("expected refusal, got {other:?}"),
    };
    assert_eq!(refused.status, "refused_needs_force");
    assert_eq!(refused.projected_nulls, 1, "`oops` has no BIGINT reading");
    assert_eq!(h.pinned_type("dur").await, "VARCHAR", "corpus untouched");
    assert!(!trawl_server::repin::marker_path(&h.data_dir).exists());

    let started = match h
        .schema_admin
        .schema_repin("dur", "BIGINT", None, false, true, RepinCeilings::default())
        .await
        .expect("forced execute")
    {
        RepinStart::Started(job) => job,
        other => panic!("expected started, got {other:?}"),
    };
    let done = h.wait_terminal(started.id).await;
    assert_eq!(done.status, "succeeded", "error: {:?}", done.error);

    // `wait_terminal` returns on the FIRST status that is not `running`, and
    // this is the very next request: no poll, no retry. A repin that records
    // its losses after the flip would fail here.
    let conflicts = h
        .query
        .catalog_conflicts(Some("dur"), None, None, None)
        .await
        .expect("conflicts");
    common::assert_bookkeeping_quiet(
        &timeouts_before,
        &common::bookkeeping_timeouts(&h.server.url).await,
    );
    assert!(
        conflicts
            .conflicts
            .iter()
            .any(|c| c.expected_type == "BIGINT" && c.rows_nulled == 1),
        "the read that first sees `succeeded` sees the loss: {:?}",
        conflicts.conflicts
    );

    assert_eq!(done.rows_nulled, 1);
    assert_eq!(h.pinned_type("dur").await, "BIGINT");

    assert_eq!(
        h.count("last=1h | where dur == 12 | stats count()").await,
        1
    );
    assert_eq!(h.count("oops last=1h | stats count()").await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn late_arriving_loss_refuses_the_cutover_without_force() {
    let h = harness().await;

    // Pin VARCHAR on a text value, then retire that file: what remains is
    // an all-numeric-text corpus, so the scan projects no loss at all.
    h.ingest_and_compact(&[event("seed", &json!({"dur": "oops"}))])
        .await;
    assert_eq!(h.pinned_type("dur").await, "VARCHAR");
    let seed_file = walk(&h.data_dir)
        .into_iter()
        .find(|p| p.file_name().is_some_and(|n| n == "seed.parquet"))
        .expect("seed.parquet exists");
    std::fs::remove_file(&seed_file).unwrap();

    // Enough affected files that the build is still running when the late
    // event lands.
    let services = ["api", "web", "worker", "edge", "db", "cache"];
    for svc in services {
        h.ingest_and_compact(&[event(svc, &json!({"dur": "12"}))])
            .await;
    }
    let dry = match h
        .schema_admin
        .schema_repin("dur", "BIGINT", None, true, false, RepinCeilings::default())
        .await
        .expect("dry run")
    {
        RepinStart::Report(job) => job,
        other => panic!("expected a dry-run report, got {other:?}"),
    };
    assert_eq!(dry.projected_nulls, 0, "the scanned corpus is all numeric");

    trawl_server::repin::engine::TEST_FILE_DELAY_MS
        .store(400, std::sync::atomic::Ordering::Relaxed);
    let started = match h
        .schema_admin
        .schema_repin(
            "dur",
            "BIGINT",
            None,
            false,
            false,
            RepinCeilings::default(),
        )
        .await
        .expect("execute")
    {
        RepinStart::Started(job) => job,
        other => panic!("expected started, got {other:?}"),
    };

    // A value the new pin cannot read, ingested and compacted during the
    // build: the catch-up folds it in and the rewrite would null it.
    h.ingest_and_compact(&[event("api", &json!({"dur": "nope"}))])
        .await;

    let done = h.wait_terminal(started.id).await;
    trawl_server::repin::engine::TEST_FILE_DELAY_MS.store(0, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        done.status, "refused_needs_force",
        "loss that appeared after the scan still needs force (error: {:?})",
        done.error
    );
    assert!(done.rows_nulled > 0, "the refusal reports the actual loss");

    assert_eq!(h.pinned_type("dur").await, "VARCHAR");
    assert_eq!(h.count("last=1h | stats count()").await, 7);
    assert_eq!(
        h.count("last=1h | where dur == \"nope\" | stats count()")
            .await,
        1
    );
    assert!(!trawl_server::repin::shadow_root(&h.data_dir).exists());
    assert!(!trawl_server::repin::aside_root(&h.data_dir).exists());
    assert!(!trawl_server::repin::marker_path(&h.data_dir).exists());

    h.wait_idle().await;
    let forced = match h
        .schema_admin
        .schema_repin("dur", "BIGINT", None, false, true, RepinCeilings::default())
        .await
        .expect("forced execute")
    {
        RepinStart::Started(job) => job,
        other => panic!("expected started, got {other:?}"),
    };
    let done = h.wait_terminal(forced.id).await;
    assert_eq!(done.status, "succeeded", "error: {:?}", done.error);
    assert_eq!(done.rows_nulled, 1);
    assert_eq!(h.pinned_type("dur").await, "BIGINT");
    assert_eq!(h.count("last=1h | stats count()").await, 7);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_disconnected_caller_does_not_strand_the_running_slot() {
    use std::sync::atomic::Ordering;
    use trawl_server::repin::engine::TEST_SCAN_HELD as SCAN_HELD;

    let h = hang_guard_harness().await;
    for svc in ["api", "web", "worker"] {
        h.ingest_and_compact(&[event(svc, &json!({"status": 200}))])
            .await;
    }

    let engine = h
        .server
        .state
        .repin
        .clone()
        .expect("an ingest-enabled node owns a repin engine");
    let hold = Hold::in_scan();
    let requester = op();
    let mut start = Box::pin(engine.start(
        "status",
        "VARCHAR",
        None,
        true,
        false,
        RequestedCeilings::default(),
        Some(&requester),
    ));
    // Waits for: the claim to land and the scan to reach its first file,
    // where the hold pins it. Then the future is dropped exactly as hyper
    // drops a handler whose connection went away.
    if let Some(early) = until_polling("the scan reaches its first file", &mut start, || {
        SCAN_HELD.load(Ordering::SeqCst)
    })
    .await
    {
        panic!("the scan must still be running when the caller disconnects: {early:?}");
    }
    drop(start);
    hold.release();

    let store = h.server.state.storage.repin.clone();
    let id = store
        .latest()
        .await
        .expect("latest job")
        .expect("the claim left a job row")
        .id;
    let started = std::time::Instant::now();
    let mut terminal = None;
    while started.elapsed() < HANG_GUARD {
        let job = store.get(id).await.expect("job row").expect("job");
        if job.status != trawl_server::store::RepinJobStatus::Running {
            terminal = Some(job);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let done = terminal.expect("the detached job must terminalize without its caller");
    assert_eq!(
        done.status,
        trawl_server::store::RepinJobStatus::Succeeded,
        "error: {:?}",
        done.error
    );

    h.wait_idle().await;
    match h
        .schema_admin
        .schema_repin(
            "status",
            "VARCHAR",
            None,
            true,
            false,
            RepinCeilings::default(),
        )
        .await
        .expect("the running slot is free again")
    {
        RepinStart::Report(_) => {}
        other => panic!("expected a dry-run report, got {other:?}"),
    }
}

/// Synchronised by ORDERING, not by timing. The build HOLDS at its first
/// published progress until this test releases it (progress is published
/// per PASS), and the query across the cutover is ordered by a second hold
/// past the point of no return.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn ingest_queries_and_a_second_repin_ride_through_a_slow_rewrite() {
    let h = hang_guard_harness().await;

    // Seed a few affected files across services, so the build's first pass
    // has real per-file progress to publish.
    for svc in ["api", "web", "worker"] {
        h.ingest_and_compact(&[
            event(svc, &json!({"status": 200})),
            event(svc, &json!({"status": 404})),
        ])
        .await;
    }
    assert_eq!(h.count("last=1h | stats count()").await, 6);

    // Arm the hold before the request: from its first published progress
    // until this test releases it, the job cannot terminalize.
    let progress_hold = Hold::at_progress();

    let started = match h
        .schema_admin
        .schema_repin(
            "status",
            "VARCHAR",
            None,
            false,
            false,
            RepinCeilings::default(),
        )
        .await
        .expect("execute")
    {
        RepinStart::Started(job) => job,
        other => panic!("expected started, got {other:?}"),
    };

    let second = h
        .schema_admin
        .schema_repin(
            "status",
            "DOUBLE",
            None,
            false,
            false,
            RepinCeilings::default(),
        )
        .await;
    match second {
        Err(trawl_client::ClientError::Server { status, .. }) => assert_eq!(status, 409),
        other => panic!("expected a 409 error envelope, got {other:?}"),
    }

    let resp = h
        .ingest
        .ingest(&[event("api", &json!({"status": 418}))])
        .await
        .expect("ingest during rewrite");
    assert_eq!(resp.accepted, 1);
    assert_eq!(
        h.count("last=1h | stats count()").await,
        7,
        "the in-flight event is queryable from the hot buffer during the build"
    );
    // The compact tick runs under the corpus gate, so it can never straddle
    // the cutover.
    h.compact_tick().await;

    await_barrier(
        &TEST_PROGRESS_PUBLISHED,
        "the build never published progress",
    )
    .await;
    let held = h
        .server
        .state
        .storage
        .repin
        .get(started.id)
        .await
        .expect("job row")
        .expect("job");
    assert_eq!(
        held.status,
        trawl_server::store::RepinJobStatus::Running,
        "the held job is still running, which is what makes the scrape mid-job"
    );
    assert_eq!(h.count("last=1h | stats count()").await, 7);
    let mid_job = scrape_metrics(&h.server.url).await;

    assert_eq!(
        gauge_value(&mid_job, trawl_server::metrics::CATALOG_REPIN_RUNNING),
        Some(1.0),
        "the running gauge must be up while the job is held"
    );
    let total_files = gauge_value(&mid_job, trawl_server::metrics::CATALOG_REPIN_FILES_TOTAL)
        .expect("mid-job scrape must carry the planned file count");
    let done_files = gauge_value(&mid_job, trawl_server::metrics::CATALOG_REPIN_FILES_DONE)
        .expect("mid-job scrape must carry the per-file progress gauge");
    assert!(
        (1.0..=total_files).contains(&done_files),
        "mid-job progress {done_files} outside 1..={total_files}"
    );
    assert!(
        !mid_job.contains("trawl_catalog_repin_jobs_total{outcome=\"succeeded\"}"),
        "the outcome counter must land only at the terminal state"
    );
    let succeeded_before = counter_value(
        &mid_job,
        trawl_server::metrics::CATALOG_REPIN_JOBS_TOTAL,
        "outcome=\"succeeded\"",
    )
    .unwrap_or(0.0);

    // They run while the progress hold pins the job mid-build, so each one has
    // given back its permit before the cutover starts its drain.
    let client = HttpClient::new_insecure(&h.server.url, &h.server.analyst_token).unwrap();
    for _ in 0..3 {
        let result = client
            .query_paginated("last=1h | stats count()", None, None)
            .await;
        assert!(result.is_ok(), "query errored mid-repin: {result:?}");
    }

    // No query may be in flight while the cutover drains: the drain is bounded
    // by the real-time `CUTOVER_DRAIN_TIMEOUT` (30 s), and a starved query
    // would end a correct job `blocked`. So the query starts only after the
    // drain, with the cutover held past its point of no return.
    let cutover_hold = Hold::past_no_return();
    progress_hold.release();
    // Waits for: the cutover's arrival past its point of no return, which
    // follows `pool.exclusive` and precedes `drop(pool_guard)`.
    await_barrier(&TEST_PAST_NO_RETURN, "the job never reached its cutover").await;
    let pool = h.server.state.query.pool.clone();
    assert_eq!(
        pool.available_permits(),
        0,
        "the held cutover holds every executor permit"
    );
    let across = pool.execute(
        pool.allocate_query_id(),
        "status=418 last=1h | stats count() by status",
        Deadline::after(HANG_GUARD),
        false,
        WorkContext::system(WorkKind::Query),
    );
    let mut across = std::pin::pin!(across);
    // The anchored negative. The query lane's first poll runs everything
    // up to its permit wait: the capacity check and the semaphore enqueue.
    // A lane that answers instead of waiting answers on that poll, so a
    // pending first poll means the query is queued behind the cutover.
    if let Some(early) = until_polling("the query's first poll", &mut across, || true).await {
        panic!("the query answered while the cutover held every permit: {early:?}");
    }
    // Waits for: one metrics round trip, with the query polled throughout.
    // The cutover hold pins the exclusion across it, so a correct lane is
    // still waiting at its end.
    let url = h.server.url.clone();
    let scrape = tokio::spawn(async move { scrape_metrics(&url).await });
    if let Some(early) =
        until_polling("a metrics round trip", &mut across, || scrape.is_finished()).await
    {
        panic!("the query answered while the cutover held every permit: {early:?}");
    }
    scrape.await.expect("the scrape task");
    assert_eq!(
        pool.available_permits(),
        0,
        "the cutover still held every permit while the query waited"
    );

    cutover_hold.release();
    let answered = until_polling("the query across the cutover", &mut across, || false)
        .await
        .expect("until_polling returns only once the future completes")
        .result
        .expect("a query queued behind the cutover waits, it does not error");
    let status_col = answered
        .columns
        .iter()
        .position(|c| c.name == "status")
        .expect("a status column");
    let count_col = answered
        .columns
        .iter()
        .position(|c| c.name != "status")
        .expect("a count column");
    assert_eq!(answered.rows.len(), 1, "{answered:?}");
    assert_eq!(
        answered.rows[0][status_col],
        trawl_engine::value::Value::String("418".to_owned()),
        "the query across the cutover reads the repinned field as VARCHAR"
    );
    assert_eq!(
        answered.rows[0][count_col],
        trawl_engine::value::Value::Integer(1)
    );

    let done = h.wait_terminal(started.id).await;
    assert_eq!(done.status, "succeeded", "error: {:?}", done.error);
    // The sweep, the gauge reset and the outcome count all precede the
    // generation guard's drop, so idle is the event that makes the scrape
    // below a fact about order.
    h.await_jobs_idle().await;

    assert_eq!(h.pinned_type("status").await, "VARCHAR");
    assert_eq!(h.count("last=1h | stats count()").await, 7);
    assert_eq!(h.count("status=418 last=1h | stats count()").await, 1);

    let metrics = scrape_metrics(&h.server.url).await;
    assert_eq!(
        gauge_value(&metrics, trawl_server::metrics::CATALOG_REPIN_RUNNING),
        Some(0.0),
        "the running gauge must fall back to 0 at the terminal state"
    );
    let succeeded_after = counter_value(
        &metrics,
        trawl_server::metrics::CATALOG_REPIN_JOBS_TOTAL,
        "outcome=\"succeeded\"",
    )
    .expect("outcome counter missing");
    assert!(
        (succeeded_after - succeeded_before - 1.0).abs() < f64::EPSILON,
        "exactly one succeeded outcome counted across the job: {succeeded_before} -> {succeeded_after}"
    );
    assert!(metrics.contains("trawl_catalog_repin_duration_seconds"));
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn events_ingested_during_the_final_pause_stay_visible_exactly_once() {
    let h = harness().await;
    let hot = h
        .server
        .state
        .query
        .hot_buffer
        .clone()
        .expect("ingest-enabled server has a hot buffer");
    let coordinator = h
        .server
        .state
        .ingest
        .repin_coordinator
        .clone()
        .expect("ingest-enabled server has a repin coordinator");

    h.ingest_and_compact(&[
        event("api", &json!({"status": 200})),
        event("api", &json!({"status": 404})),
    ])
    .await;
    assert_eq!(h.count("last=1h | stats count()").await, 2);
    assert_eq!(
        hot.event_count(),
        0,
        "the seed batch drained after compaction"
    );

    coordinator.set_cutover_hold_ms(3_000);
    let started = match h
        .schema_admin
        .schema_repin(
            "status",
            "VARCHAR",
            None,
            false,
            false,
            RepinCeilings::default(),
        )
        .await
        .expect("execute")
    {
        RepinStart::Started(job) => job,
        other => panic!("expected started, got {other:?}"),
    };

    let mut entered = false;
    for _ in 0..600 {
        if coordinator.cutover_hold_active() {
            entered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(entered, "the cutover never entered its pause");

    let resp = tokio::time::timeout(
        Duration::from_secs(2),
        h.ingest.ingest(&[event("api", &json!({"status": 418}))]),
    )
    .await
    .expect("ingest must not block on the cutover")
    .expect("ingest during the pause");
    assert_eq!(resp.accepted, 1);
    assert_eq!(
        hot.event_count(),
        1,
        "the event ingested during the pause is held in the hot buffer"
    );

    let paused_query = {
        let client = HttpClient::new_insecure(&h.server.url, &h.server.analyst_token).unwrap();
        tokio::spawn(async move {
            client
                .query_paginated("status=418 last=1h | stats count()", None, None)
                .await
        })
    };

    // A compaction batch that starts inside the pause — trawld's loop
    // ticking on schedule, not a test-driven tick. It blocks at the corpus
    // gate and holds its WAL files and hot batch until the pause lifts.
    let deferred_drain = {
        let wal_dir = h.wal_dir.clone();
        let data_dir = h.data_dir.clone();
        let hot = hot.clone();
        let ctx = catalog_ctx(&h.server);
        let coordinator = coordinator.clone();
        tokio::spawn(async move {
            trawl_server::ingest::compaction::compact_once_coordinated(
                &wal_dir,
                &data_dir,
                Duration::ZERO,
                Duration::ZERO,
                false,
                Some(&hot),
                500,
                "2GB",
                Some(&ctx),
                Some(&coordinator),
            )
            .await
        })
    };

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        hot.event_count(),
        1,
        "no drain may make an undrained event invisible during the pause"
    );
    assert!(
        !deferred_drain.is_finished(),
        "the batch that started inside the pause waits at the corpus gate"
    );
    assert!(
        coordinator.cutover_hold_active(),
        "the assertions above must land inside the pause window"
    );

    let done = h.wait_terminal(started.id).await;
    assert_eq!(done.status, "succeeded", "error: {:?}", done.error);

    let answered = paused_query
        .await
        .expect("query task")
        .expect("a query issued into the pause waits, it does not error");
    assert_eq!(
        answered.result.rows[0][0],
        trawl_engine::value::Value::Integer(1),
        "the paused query sees the event ingested during the pause"
    );

    assert_eq!(h.pinned_type("status").await, "VARCHAR");

    let errors = tokio::time::timeout(Duration::from_secs(30), deferred_drain)
        .await
        .expect("the batch blocked by the pause must complete once it lifts")
        .expect("compaction task")
        .expect("compaction tick must succeed");
    assert_eq!(errors, 0, "no compaction errors expected");
    assert_eq!(
        hot.event_count(),
        0,
        "the pause deferred the drain, it neither dropped nor starved it"
    );

    // Counted after the deferred batch drained: a batch in flight may briefly
    // show an event in both its new parquet and the not-yet-drained hot
    // snapshot, ADR-0008's accepted transient duplicate.
    assert_eq!(h.count("last=1h | stats count()").await, 3);
    assert_eq!(h.count("status=418 last=1h | stats count()").await, 1);
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn boot_reconciliation_completes_a_recovered_cutover() {
    let h = harness().await;

    h.ingest_and_compact(&[event("api", &json!({"status": 200}))])
        .await;
    assert_eq!(h.pinned_type("status").await, "BIGINT");

    // Simulate a crash mid-cutover on a throwaway root: job row claimed,
    // marker written in phase cutover, swap half-done.
    let job_id = h
        .server
        .state
        .storage
        .repin
        .claim(trawl_server::store::RepinClaim {
            field: "status",
            from_type: trawl_core::schema::CanonicalType::BigInt,
            to_type: trawl_core::schema::CanonicalType::Varchar,
            dialect: None,
            dry_run: false,
            force: false,
            requester: None,
            max_nulled_rows: None,
            max_ambiguous_rows: None,
        })
        .await
        .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let dir = data.join("prod/2026-01-01/10");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("svc.parquet"), b"old generation").unwrap();
    let shadow = trawl_server::repin::shadow_root(&data);
    let sdir = shadow.join("prod/2026-01-01/10");
    std::fs::create_dir_all(&sdir).unwrap();
    std::fs::write(sdir.join("svc.parquet"), b"new generation").unwrap();
    trawl_server::repin::marker::write_marker(
        &data,
        &trawl_server::repin::RepinMarker {
            job_id,
            field: "status".to_owned(),
            from_type: "BIGINT".to_owned(),
            to_type: "VARCHAR".to_owned(),
            phase: trawl_server::repin::RepinPhase::Cutover,
        },
    )
    .unwrap();

    let recovered = trawl_server::repin::recover::recover_filesystem(&data, true)
        .unwrap()
        .expect("marker present");
    trawl_server::repin::recover::reconcile_store(
        &h.server.state.storage,
        &h.server.state.query.field_catalog,
        &data,
        Some(recovered),
    )
    .await
    .expect("store reconciliation");

    assert_eq!(
        std::fs::read(data.join("prod/2026-01-01/10/svc.parquet")).unwrap(),
        b"new generation"
    );
    assert!(!trawl_server::repin::aside_root(&data).exists());
    assert!(!trawl_server::repin::marker_path(&data).exists());
    let job = h
        .server
        .state
        .storage
        .repin
        .get(job_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.status, trawl_server::store::RepinJobStatus::Succeeded);
    assert_eq!(h.pinned_type("status").await, "VARCHAR");
    assert_eq!(
        h.server.state.query.field_catalog.get("status"),
        Some(trawl_core::schema::CanonicalType::Varchar),
        "the in-process cache flipped too"
    );
    assert!(
        !h.server.state.storage.catalog.is_conformed().await.unwrap(),
        "a recovered cutover re-arms the boot conformance pass"
    );

    // A staging root that survives its sweep keeps the marker: the marker
    // is the only thing licensing trawl to delete it.
    {
        use std::os::unix::fs::PermissionsExt as _;

        let aside = trawl_server::repin::aside_root(&data);
        let stuck = aside.join("prod/2026-01-01/10");
        std::fs::create_dir_all(&stuck).unwrap();
        std::fs::write(stuck.join("svc.parquet"), b"previous generation").unwrap();
        std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o500)).unwrap();
        if std::fs::remove_file(stuck.join("svc.parquet")).is_ok() {
            // Running as root: mode bits are not enforced.
            let _ = std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o755));
            std::fs::remove_dir_all(&aside).unwrap();
        } else {
            trawl_server::repin::marker::write_marker(
                &data,
                &trawl_server::repin::RepinMarker {
                    job_id,
                    field: "status".to_owned(),
                    from_type: "BIGINT".to_owned(),
                    to_type: "VARCHAR".to_owned(),
                    phase: trawl_server::repin::RepinPhase::Cleanup,
                },
            )
            .unwrap();
            let recovered = trawl_server::repin::recover::recover_filesystem(&data, true)
                .unwrap()
                .expect("marker present");
            trawl_server::repin::recover::reconcile_store(
                &h.server.state.storage,
                &h.server.state.query.field_catalog,
                &data,
                Some(recovered),
            )
            .await
            .expect("store reconciliation");
            assert!(
                trawl_server::repin::marker_path(&data).exists(),
                "a failed aside sweep must keep the marker for the next boot"
            );
            std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::remove_dir_all(&aside).unwrap();
            trawl_server::repin::marker::remove_marker(&data).unwrap();
        }
    }

    let orphan_id = h
        .server
        .state
        .storage
        .repin
        .claim(trawl_server::store::RepinClaim {
            field: "status",
            from_type: trawl_core::schema::CanonicalType::Varchar,
            to_type: trawl_core::schema::CanonicalType::BigInt,
            dialect: None,
            dry_run: false,
            force: false,
            requester: None,
            max_nulled_rows: None,
            max_ambiguous_rows: None,
        })
        .await
        .unwrap();
    trawl_server::repin::recover::reconcile_store(
        &h.server.state.storage,
        &h.server.state.query.field_catalog,
        &data,
        None,
    )
    .await
    .expect("orphan reconciliation");
    let orphan = h
        .server
        .state
        .storage
        .repin
        .get(orphan_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(orphan.status, trawl_server::store::RepinJobStatus::Failed);
}

#[tokio::test(flavor = "multi_thread")]
// The crash state has to be built by hand (claim, stage, plant marker and
// shadow), and both boot halves run twice.
#[allow(clippy::too_many_lines)]
async fn boot_reconciliation_materialises_the_staged_evidence() {
    let h = harness().await;

    h.ingest_and_compact(&[
        event("api", &json!({"dur": "12"})),
        event("api", &json!({"dur": "oops"})),
    ])
    .await;
    assert_eq!(h.pinned_type("dur").await, "VARCHAR");

    let job_id = h
        .server
        .state
        .storage
        .repin
        .claim(trawl_server::store::RepinClaim {
            field: "dur",
            from_type: trawl_core::schema::CanonicalType::Varchar,
            to_type: trawl_core::schema::CanonicalType::BigInt,
            dialect: None,
            dry_run: false,
            force: true,
            max_nulled_rows: None,
            max_ambiguous_rows: None,
            requester: None,
        })
        .await
        .unwrap();

    // The state a crash in the flip window leaves behind: tallies staged,
    // Cutover marker down, envs half swapped. Everything after this point
    // is what the next boot does with it.
    h.server
        .state
        .storage
        .repin
        .stage_cutover_input(
            job_id,
            trawl_server::store::JobTotals {
                files_done: 1,
                rows_rewritten: 2,
                rows_nulled: 1,
                rows_resurrected: 0,
                ambiguous_numerals: 0,
            },
            &[("api".to_owned(), 1)],
        )
        .await
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let dir = data.join("prod/2026-01-01/10");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("api.parquet"), b"old generation").unwrap();
    let sdir = trawl_server::repin::shadow_root(&data).join("prod/2026-01-01/10");
    std::fs::create_dir_all(&sdir).unwrap();
    std::fs::write(sdir.join("api.parquet"), b"new generation").unwrap();
    let plant_marker = |phase| {
        trawl_server::repin::marker::write_marker(
            &data,
            &trawl_server::repin::RepinMarker {
                job_id,
                field: "dur".to_owned(),
                from_type: "VARCHAR".to_owned(),
                to_type: "BIGINT".to_owned(),
                phase,
            },
        )
        .unwrap();
    };
    plant_marker(trawl_server::repin::RepinPhase::Cutover);

    let recovered = trawl_server::repin::recover::recover_filesystem(&data, true)
        .unwrap()
        .expect("marker present");
    trawl_server::repin::recover::reconcile_store(
        &h.server.state.storage,
        &h.server.state.query.field_catalog,
        &data,
        Some(recovered),
    )
    .await
    .expect("store reconciliation");

    let job = h
        .server
        .state
        .storage
        .repin
        .get(job_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.status, trawl_server::store::RepinJobStatus::Succeeded);

    let conflicts = h
        .query
        .catalog_conflicts(Some("dur"), None, None, None)
        .await
        .expect("conflicts");
    let rows: Vec<_> = conflicts
        .conflicts
        .iter()
        .filter(|c| c.field == "dur")
        .collect();
    assert_eq!(rows.len(), 1, "one row per lossy service: {rows:?}");
    assert_eq!(rows[0].service, "api");
    assert_eq!(rows[0].rows_nulled, 1);
    assert_eq!(rows[0].observed_type, "VARCHAR");
    assert_eq!(rows[0].expected_type, "BIGINT");

    plant_marker(trawl_server::repin::RepinPhase::Cleanup);
    let recovered = trawl_server::repin::recover::recover_filesystem(&data, true)
        .unwrap()
        .expect("marker present");
    trawl_server::repin::recover::reconcile_store(
        &h.server.state.storage,
        &h.server.state.query.field_catalog,
        &data,
        Some(recovered),
    )
    .await
    .expect("store reconciliation");
    let again = h
        .query
        .catalog_conflicts(Some("dur"), None, None, None)
        .await
        .expect("conflicts");
    assert_eq!(
        again.conflicts.iter().filter(|c| c.field == "dur").count(),
        1,
        "a replayed flip must not double-insert the staged evidence"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn resurrection_only_pass_recovers_without_retyping() {
    let h = harness().await;
    h.ingest_and_compact(&[event("api", &json!({"status": 200}))])
        .await;
    h.ingest_and_compact(&[event("api", &json!({"status": "accepted"}))])
        .await;
    assert_eq!(h.pinned_type("status").await, "BIGINT");

    let err = h
        .schema_admin
        .schema_repin(
            "status",
            "BIGINT",
            None,
            false,
            false,
            RepinCeilings::default(),
        )
        .await
        .expect_err("same-type without force refuses");
    match err {
        trawl_client::ClientError::Server { status, .. } => assert_eq!(status, 400),
        other => panic!("expected 400, got {other:?}"),
    }

    let dry = match h
        .schema_admin
        .schema_repin(
            "status",
            "BIGINT",
            None,
            true,
            true,
            RepinCeilings::default(),
        )
        .await
        .expect("dry run")
    {
        RepinStart::Report(job) => job,
        other => panic!("expected report, got {other:?}"),
    };
    assert_eq!(
        dry.projected_nulls, 0,
        "already-shelved values are not new losses"
    );
    assert_eq!(dry.resurrectable, 0, "`accepted` has no BIGINT reading");
}

/// The engine writes the marker with the CATALOG spelling, so recovery has
/// to read it back that way: the physical parse has no `SEVERITY` spelling.
#[tokio::test(flavor = "multi_thread")]
async fn boot_reconciliation_replays_a_severity_cutover() {
    let h = harness().await;

    h.ingest_and_compact(&[event("api", &json!({"level": "error"}))])
        .await;
    assert_eq!(h.pinned_type("level").await, "VARCHAR");

    let job_id = h
        .server
        .state
        .storage
        .repin
        .claim(trawl_server::store::RepinClaim {
            field: "level",
            from_type: trawl_core::schema::CanonicalType::Varchar,
            to_type: trawl_core::schema::CanonicalType::Severity,
            dialect: Some(trawl_core::severity::Dialect::Syslog),
            dry_run: false,
            force: true,
            requester: None,
            max_nulled_rows: None,
            max_ambiguous_rows: None,
        })
        .await
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let dir = data.join("prod/2026-01-01/10");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("svc.parquet"), b"old generation").unwrap();
    let shadow = trawl_server::repin::shadow_root(&data);
    let sdir = shadow.join("prod/2026-01-01/10");
    std::fs::create_dir_all(&sdir).unwrap();
    std::fs::write(sdir.join("svc.parquet"), b"new generation").unwrap();
    trawl_server::repin::marker::write_marker(
        &data,
        &trawl_server::repin::RepinMarker {
            job_id,
            field: "level".to_owned(),
            from_type: trawl_core::schema::CanonicalType::Varchar
                .as_catalog()
                .to_owned(),
            to_type: trawl_core::schema::CanonicalType::Severity
                .as_catalog()
                .to_owned(),
            phase: trawl_server::repin::RepinPhase::Cutover,
        },
    )
    .unwrap();

    let recovered = trawl_server::repin::recover::recover_filesystem(&data, true)
        .unwrap()
        .expect("marker present");
    trawl_server::repin::recover::reconcile_store(
        &h.server.state.storage,
        &h.server.state.query.field_catalog,
        &data,
        Some(recovered),
    )
    .await
    .expect("a severity cutover marker must replay, not refuse");

    assert_eq!(
        std::fs::read(data.join("prod/2026-01-01/10/svc.parquet")).unwrap(),
        b"new generation"
    );
    assert_eq!(h.pinned_type("level").await, "SEVERITY");
    assert_eq!(
        h.server.state.query.field_catalog.get("level"),
        Some(trawl_core::schema::CanonicalType::Severity),
        "the cache must hold the SEMANTIC pin, not the BIGINT under it"
    );
    let job = h
        .server
        .state
        .storage
        .repin
        .get(job_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.status, trawl_server::store::RepinJobStatus::Succeeded);
    assert_eq!(job.dialect.as_deref(), Some("syslog"));
    assert!(!trawl_server::repin::marker_path(&data).exists());
}

async fn dry_run(
    client: &HttpClient,
    field: &str,
    to: &str,
    dialect: Option<&str>,
    force: bool,
) -> trawl_client::RepinJobResponse {
    match client
        .schema_repin(field, to, dialect, true, force, RepinCeilings::default())
        .await
        .expect("dry run")
    {
        RepinStart::Report(job) => job,
        other => panic!("expected a report, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dry_run_reports_the_force_verdict_it_would_hit() {
    let h = harness().await;

    // `level` carries a value no ladder rung reads (loss), `pri` carries a
    // numeral both dialects read differently (ambiguity) and nothing else.
    h.ingest_and_compact(&[
        event("api", &json!({"level": "error", "pri": "error"})),
        event("api", &json!({"level": "gold", "pri": "3"})),
    ])
    .await;
    assert_eq!(h.pinned_type("level").await, "VARCHAR");
    assert_eq!(h.pinned_type("pri").await, "VARCHAR");

    let dry = dry_run(&h.schema_admin, "level", "severity", None, false).await;
    assert_eq!(dry.status, "succeeded", "a dry run still succeeds");
    assert_eq!(dry.projected_nulls, 1);
    assert_eq!(
        dry.requires_force,
        Some(true),
        "a plan that would null a value must say so: {dry:?}"
    );
    let reason = dry.requires_force_reason.clone().expect("a reason");
    assert!(reason.contains("cannot be read as SEVERITY"), "{reason}");
    assert_eq!(
        dry.unmapped_samples,
        vec!["gold".to_owned()],
        "the report shows WHICH value it cannot read"
    );

    let latest = h
        .schema_admin
        .schema_repin_status()
        .await
        .expect("status")
        .job
        .expect("a job has run");
    assert_eq!(latest.id, dry.id);
    assert_eq!(latest.requires_force, Some(true));
    assert_eq!(latest.requires_force_reason, dry.requires_force_reason);

    // (2) Ambiguity: nothing is lost, but `3` means err to syslog and
    // trace3 to OTel — the gate fires on the default OTel reading.
    let dry = dry_run(&h.schema_admin, "pri", "severity", None, false).await;
    assert_eq!(dry.projected_nulls, 0, "every value has an OTel reading");
    assert_eq!(dry.ambiguous_numerals, 1);
    assert_eq!(dry.requires_force, Some(true), "{dry:?}");
    let reason = dry.requires_force_reason.clone().expect("a reason");
    assert!(reason.contains("numeral 1-7"), "{reason}");
    assert!(reason.contains("dialect=syslog"), "{reason}");

    let dry = dry_run(&h.schema_admin, "pri", "severity", Some("syslog"), false).await;
    assert_eq!(dry.dialect.as_deref(), Some("syslog"));
    assert_eq!(dry.ambiguous_numerals, 1, "the COUNT is dialect-blind");
    assert_eq!(
        dry.requires_force,
        Some(false),
        "an asserted dialect IS the answer to the ambiguity: {dry:?}"
    );
    let forced = dry_run(&h.schema_admin, "pri", "severity", None, true).await;
    assert_eq!(
        forced.requires_force,
        Some(false),
        "force clears the gate: {forced:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_force_verdict_is_absent_until_the_scan_has_a_plan() {
    for force in [false, true] {
        let h = harness().await;
        h.ingest_and_compact(&[event("api", &json!({"level": "error", "dur": 12}))])
            .await;
        h.ingest_and_compact(&[event("web", &json!({"level": "gold", "dur": 34}))])
            .await;

        // Slow the scan so the claimed-but-unplanned window is observable.
        trawl_server::repin::engine::TEST_SCAN_DELAY_MS
            .store(500, std::sync::atomic::Ordering::Relaxed);
        let client = h.schema_admin.clone();
        let dry = tokio::spawn(async move {
            client
                .schema_repin(
                    "level",
                    "severity",
                    None,
                    true,
                    force,
                    RepinCeilings::default(),
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let running = h
            .schema_admin
            .schema_repin_status()
            .await
            .expect("status")
            .job
            .expect("a job is claimed");
        assert_eq!(running.status, "running");
        assert_eq!(
            running.requires_force, None,
            "a job with no recorded plan has no verdict to report: {running:?}"
        );
        assert_eq!(running.requires_force_reason, None);

        let dry = match dry.await.expect("join").expect("dry run") {
            RepinStart::Report(job) => job,
            other => panic!("expected a report, got {other:?}"),
        };
        trawl_server::repin::engine::TEST_SCAN_DELAY_MS
            .store(0, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(dry.requires_force, Some(!force), "{dry:?}");
        let latest = h
            .schema_admin
            .schema_repin_status()
            .await
            .expect("status")
            .job
            .expect("a job has run");
        assert_eq!(latest.requires_force, Some(!force));

        let clean = dry_run(&h.schema_admin, "dur", "varchar", None, false).await;
        assert_eq!(
            clean.requires_force,
            Some(false),
            "VARCHAR is the always-lossless target: {clean:?}"
        );
        assert_eq!(clean.requires_force_reason, None);
    }
}

/// One sender field carrying the five shapes a severity repin has to answer
/// for: two case-variant tokens, an exact `OTel` short name, a
/// dialect-ambiguous numeral, and a value no ladder rung reads.
fn severity_corpus() -> Vec<serde_json::Value> {
    ["error", "ERROR", "error2", "3", "gold"]
        .iter()
        .map(|level| event("api", &json!({"level": level})))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn repin_to_severity_dry_run_matches_the_executed_rewrite() {
    let h = harness().await;
    h.ingest_and_compact(&severity_corpus()).await;
    assert_eq!(h.pinned_type("level").await, "VARCHAR");
    assert_eq!(h.count("last=1h | stats count()").await, 5);

    let dry = dry_run(&h.schema_admin, "level", "severity", None, false).await;
    assert_eq!(dry.files_total, 1);
    assert_eq!(dry.rows_carrying, 5, "every row carries a level");
    assert_eq!(dry.projected_nulls, 1, "only `gold` has no reading");
    assert_eq!(dry.resurrectable, 0, "nothing was shelved before this");
    assert_eq!(
        dry.ambiguous_numerals, 1,
        "`3` reads differently per dialect"
    );
    assert_eq!(
        dry.dialect.as_deref(),
        Some("otel"),
        "the default assertion"
    );
    assert_eq!(dry.unmapped_samples, vec!["gold".to_owned()]);
    let live = dry.liveness.as_ref().expect("the sender just wrote");
    assert_eq!(live.service, "api");
    assert_eq!(
        dry.requires_force,
        Some(true),
        "loss AND ambiguity, neither accepted yet"
    );

    let started = match h
        .schema_admin
        .schema_repin(
            "level",
            "severity",
            None,
            false,
            true,
            RepinCeilings::default(),
        )
        .await
        .expect("execute")
    {
        RepinStart::Started(job) => job,
        other => panic!("expected started, got {other:?}"),
    };
    let done = h.wait_terminal(started.id).await;
    assert_eq!(done.status, "succeeded", "error: {:?}", done.error);
    assert_eq!(done.files_done, dry.files_total);
    assert_eq!(done.rows_rewritten, dry.rows_carrying);
    assert_eq!(
        done.rows_nulled, dry.projected_nulls,
        "the rewrite nulled exactly what the dry run projected"
    );
    assert_eq!(done.rows_resurrected, dry.resurrectable);
    assert_eq!(
        done.ambiguous_numerals, dry.ambiguous_numerals,
        "the shadow saw the same dialect-ambiguous rows the scan projected"
    );
    assert_eq!(h.pinned_type("level").await, "SEVERITY");

    // The pin now gives the sender's own field the ladder's vocabulary:
    // bands for equality, the exact number when ordered, and `gold` is a
    // NULL the pin cannot hold (its original stays in _raw).
    assert_eq!(h.count("level=error last=1h | stats count()").await, 3);
    assert_eq!(h.count("level>=warn last=1h | stats count()").await, 3);
    assert_eq!(
        h.count("level=error2 last=1h | stats count()").await,
        1,
        "an exact short name is exact, not its band"
    );
    assert_eq!(
        h.count("level=trace3 last=1h | stats count()").await,
        1,
        "the numeral 3 conformed as the OTel rung it names"
    );
    assert_eq!(h.count("level=* last=1h | stats count()").await, 4);
    assert_eq!(
        h.count("\"gold\" last=1h | stats count()").await,
        1,
        "the unreadable original is still findable in _raw"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_syslog_repin_inverts_the_ladder_and_persists_the_assertion() {
    let h = harness().await;
    h.ingest_and_compact(&severity_corpus()).await;

    // The ambiguity gate does not fire under an explicit syslog assertion,
    // but `gold` is still a loss, so this run is forced for that reason.
    let started = match h
        .schema_admin
        .schema_repin(
            "level",
            "severity",
            Some("syslog"),
            false,
            true,
            RepinCeilings::default(),
        )
        .await
        .expect("execute")
    {
        RepinStart::Started(job) => job,
        other => panic!("expected started, got {other:?}"),
    };
    let done = h.wait_terminal(started.id).await;
    assert_eq!(done.status, "succeeded", "error: {:?}", done.error);
    assert_eq!(
        done.dialect.as_deref(),
        Some("syslog"),
        "the assertion is persisted on the job row"
    );

    // Syslog counts down: `3` is err (17), not trace3 (3). Words are
    // dialect-free, so the three tokens read exactly as before.
    assert_eq!(
        h.count("level=error last=1h | stats count()").await,
        4,
        "error, ERROR, error2 and the inverted `3` are all in the error band"
    );
    assert_eq!(h.count("level=trace3 last=1h | stats count()").await, 0);
    assert_eq!(h.count("level>=warn last=1h | stats count()").await, 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn resurrection_recovers_a_shelved_token_under_a_case_variant_key() {
    let h = harness().await;

    h.ingest_and_compact(&[event("api", &json!({"lvl": 17}))])
        .await;
    assert_eq!(h.pinned_type("lvl").await, "BIGINT");
    // The token batch conflicts: the value lives on in `_raw`, which holds the
    // sender's original key spelling (`Lvl`), because `_raw` is captured before
    // the name fold.
    h.ingest_and_compact(&[event("api", &json!({"Lvl": "error"}))])
        .await;
    assert_eq!(
        h.count("lvl=* last=1h | stats count()").await,
        1,
        "the token was shelved by the BIGINT pin"
    );

    let dry = dry_run(&h.schema_admin, "lvl", "severity", None, true).await;
    assert_eq!(
        dry.resurrectable, 1,
        "the shelved `error` is recoverable from _raw under a case-variant key"
    );
    assert_eq!(dry.projected_nulls, 0, "17 is already a ladder position");

    let started = match h
        .schema_admin
        .schema_repin(
            "lvl",
            "severity",
            None,
            false,
            true,
            RepinCeilings::default(),
        )
        .await
        .expect("execute")
    {
        RepinStart::Started(job) => job,
        other => panic!("expected started, got {other:?}"),
    };
    let done = h.wait_terminal(started.id).await;
    assert_eq!(done.status, "succeeded", "error: {:?}", done.error);
    assert_eq!(done.rows_resurrected, 1);
    assert_eq!(h.pinned_type("lvl").await, "SEVERITY");
    assert_eq!(
        h.count("lvl=error last=1h | stats count()").await,
        2,
        "the stored 17 and the resurrected `error` are one value now"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn post_repin_severity_binds_in_every_lane_including_live_ingest() {
    let h = harness().await;
    h.ingest_and_compact(&severity_corpus()).await;
    let started = match h
        .schema_admin
        .schema_repin(
            "level",
            "severity",
            None,
            false,
            true,
            RepinCeilings::default(),
        )
        .await
        .expect("execute")
    {
        RepinStart::Started(job) => job,
        other => panic!("expected started, got {other:?}"),
    };
    let done = h.wait_terminal(started.id).await;
    assert_eq!(done.status, "succeeded", "error: {:?}", done.error);

    assert_eq!(h.count("level>=warn last=1h | stats count()").await, 3);

    let live = event("api", &json!({"level": "error"}));
    assert_eq!(
        h.ingest
            .ingest(std::slice::from_ref(&live))
            .await
            .unwrap()
            .accepted,
        1
    );
    assert_eq!(
        h.count("level>=warn last=1h | stats count()").await,
        4,
        "the hot row reads on the ladder before compaction"
    );
    assert_eq!(
        h.count("last=1h | where level >= \"error\" | stats count()")
            .await,
        4
    );

    // The live-tail lane: the same filter the SSE stream compiles, over the
    // same catalog snapshot the handler hands it.
    let pins = h.server.state.query.field_catalog.all();
    let matches = |dsl: &str| -> bool {
        let ast = trawl_core::parser::parse(dsl).expect("parses");
        let filter =
            trawl_core::filter::CompiledFilter::compile(&ast.search, &pins).expect("compiles");
        let event: serde_json::Map<String, serde_json::Value> =
            live.as_object().cloned().expect("an object");
        filter
            .matches_at(&event, &trawl_core::context::EvalContext::capture())
            .expect("a never stop never cancels")
    };
    assert!(matches("level>=warn"), "the live tail binds the new pin");
    assert!(matches("level=error"));
    assert!(!matches("level=trace3"));

    h.compact_tick().await;
    assert_eq!(h.count("level>=warn last=1h | stats count()").await, 4);
    assert_eq!(h.count("level=error last=1h | stats count()").await, 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_catch_up_pass_rides_the_jobs_dialect_while_live_ingest_stays_otel() {
    let h = hang_guard_harness().await;
    h.ingest_and_compact(&[event("api", &json!({"level": "error"}))])
        .await;
    assert_eq!(h.pinned_type("level").await, "VARCHAR");

    // A barrier, not just a delay: pass 0 takes its source snapshot and then
    // waits, so the file this test writes next provably did not exist when
    // the build enumerated its sources. Only a catch-up pass can carry it.
    let hold = Hold::at_first_pass();

    let started = match h
        .schema_admin
        .schema_repin(
            "level",
            "severity",
            Some("syslog"),
            false,
            true,
            RepinCeilings::default(),
        )
        .await
        .expect("execute")
    {
        RepinStart::Started(job) => job,
        other => panic!("expected started, got {other:?}"),
    };

    await_barrier(
        &TEST_SNAPSHOT_TAKEN,
        "the build never reached its first snapshot",
    )
    .await;
    // Written by compaction under the old pin, after that snapshot.
    h.ingest_and_compact(&[event("api", &json!({"level": "3"}))])
        .await;
    hold.release();

    let done = h.wait_terminal(started.id).await;
    assert_eq!(done.status, "succeeded", "error: {:?}", done.error);
    // The mid-build row survived the swap, which only a catch-up pass can
    // do: the cutover publishes the shadow wholesale, so a file the build
    // never folded in would be gone.
    assert_eq!(h.count("last=1h | stats count()").await, 2);

    assert_eq!(
        h.count("level=error last=1h | stats count()").await,
        2,
        "the token and the inverted numeral are both in the error band"
    );
    assert_eq!(h.count("level=trace3 last=1h | stats count()").await, 0);

    h.ingest_and_compact(&[event("api", &json!({"level": "3"}))])
        .await;
    assert_eq!(
        h.count("level=trace3 last=1h | stats count()").await,
        1,
        "a live `3` reads as the OTel rung it names; the repin translated \
         HISTORY only"
    );
    assert_eq!(h.count("level=error last=1h | stats count()").await, 2);
}

use std::sync::atomic::{AtomicBool, Ordering};

use trawl_client::RepinCancel;
use trawl_client::RepinJobResponse;
use trawl_server::repin::engine::{
    TEST_BARRIER_FIRST_PASS, TEST_FORCE_REFUSAL_REACHED, TEST_HOLD_AFTER_NO_RETURN,
    TEST_HOLD_AFTER_PROGRESS, TEST_HOLD_AT_FORCE_REFUSAL, TEST_HOLD_IN_SCAN, TEST_PAST_NO_RETURN,
    TEST_PROGRESS_PUBLISHED, TEST_RELEASE_BUILD, TEST_RELEASE_CUTOVER, TEST_RELEASE_FORCE_REFUSAL,
    TEST_RELEASE_JOB, TEST_RELEASE_SCAN, TEST_SCAN_HELD, TEST_SNAPSHOT_TAKEN,
};
use trawl_server::store::{RepinJob, RepinJobStatus};

/// A hold waits only for its release flag: the engine has no clock lapse
/// that frees it. Dropping it unreleased disarms a hold the engine has not
/// reached and sets the release flag for one it has, so a panicking test
/// never leaves a job (or a blocking-pool thread the runtime waits for on
/// teardown) held. The flags are process-global, so arming resets the
/// arrival and release flags and sets the hold last.
///
/// Bind the guard as a local of the test body, so unwinding drops it before
/// the runtime does.
struct Hold {
    armed: &'static AtomicBool,
    release: &'static AtomicBool,
    released: bool,
}

impl Hold {
    fn arm(
        armed: &'static AtomicBool,
        arrived: &'static AtomicBool,
        release: &'static AtomicBool,
    ) -> Self {
        arrived.store(false, Ordering::SeqCst);
        release.store(false, Ordering::SeqCst);
        armed.store(true, Ordering::SeqCst);
        Self {
            armed,
            release,
            released: false,
        }
    }

    fn at_progress() -> Self {
        Self::arm(
            &TEST_HOLD_AFTER_PROGRESS,
            &TEST_PROGRESS_PUBLISHED,
            &TEST_RELEASE_JOB,
        )
    }

    fn in_scan() -> Self {
        Self::arm(&TEST_HOLD_IN_SCAN, &TEST_SCAN_HELD, &TEST_RELEASE_SCAN)
    }

    fn past_no_return() -> Self {
        Self::arm(
            &TEST_HOLD_AFTER_NO_RETURN,
            &TEST_PAST_NO_RETURN,
            &TEST_RELEASE_CUTOVER,
        )
    }

    fn at_force_refusal() -> Self {
        Self::arm(
            &TEST_HOLD_AT_FORCE_REFUSAL,
            &TEST_FORCE_REFUSAL_REACHED,
            &TEST_RELEASE_FORCE_REFUSAL,
        )
    }

    fn at_first_pass() -> Self {
        Self::arm(
            &TEST_BARRIER_FIRST_PASS,
            &TEST_SNAPSHOT_TAKEN,
            &TEST_RELEASE_BUILD,
        )
    }

    fn release(mut self) {
        self.released = true;
        self.release.store(true, Ordering::SeqCst);
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        if !self.released {
            self.armed.store(false, Ordering::SeqCst);
            self.release.store(true, Ordering::SeqCst);
        }
    }
}

async fn await_barrier(flag: &AtomicBool, what: &str) {
    let start = std::time::Instant::now();
    while start.elapsed() < HANG_GUARD {
        if flag.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{what}");
}

fn corpus_digest(data_dir: &std::path::Path) -> std::collections::BTreeMap<String, (u64, u64)> {
    use std::hash::{Hash as _, Hasher as _};

    walk(data_dir)
        .into_iter()
        .map(|path| {
            let bytes = std::fs::read(&path).expect("read a corpus file");
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            bytes.hash(&mut hasher);
            let rel = path
                .strip_prefix(data_dir)
                .expect("a corpus path under the data root")
                .display()
                .to_string();
            (rel, (bytes.len() as u64, hasher.finish()))
        })
        .collect()
}

fn counter_value(scrape: &str, name: &str, labels: &str) -> Option<f64> {
    let prefix = format!("{name}{{{labels}}}");
    scrape.lines().find_map(|line| {
        let rest = line.strip_prefix(&prefix)?.strip_prefix(' ')?;
        rest.trim().parse().ok()
    })
}

impl Harness {
    async fn repin_row(&self, id: i64) -> RepinJob {
        self.server
            .state
            .storage
            .repin
            .get(id)
            .await
            .expect("job row")
            .expect("job")
    }

    async fn await_cancel_request(&self, id: i64) -> chrono::DateTime<chrono::Utc> {
        let start = std::time::Instant::now();
        while start.elapsed() < HANG_GUARD {
            if let Some(at) = self.repin_row(id).await.cancel_requested_at {
                return at;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the cancel request never reached the job row within {HANG_GUARD:?}");
    }

    async fn await_retention_gauge(&self) -> f64 {
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let _retention = trawl_server::retention::spawn_retention(
            self.data_dir.clone(),
            self.wal_dir.clone(),
            std::sync::Arc::new(trawl_server::retention::RetentionShared::new(
                trawl_server::config::RetentionConfig {
                    max_age_days: 90,
                    min_free_disk_bytes: 0,
                    retention_interval_secs: 1,
                    env: std::collections::BTreeMap::new(),
                },
            )),
            rx,
        );
        let start = std::time::Instant::now();
        while start.elapsed() < HANG_GUARD {
            if let Some(value) = gauge_value(
                &scrape_metrics(&self.server.url).await,
                trawl_server::metrics::RETENTION_SUPPRESSED,
            ) {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("retention never published its suppression gauge within {HANG_GUARD:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one cancel, and everything it must leave alone
async fn a_mid_build_cancel_leaves_the_corpus_and_the_staging_untouched() {
    let h = hang_guard_harness().await;
    for svc in ["api", "web", "worker"] {
        h.ingest_and_compact(&[
            event(svc, &json!({"status": 200})),
            event(svc, &json!({"status": 404})),
        ])
        .await;
    }
    assert_eq!(h.pinned_type("status").await, "BIGINT");
    let before = corpus_digest(&h.data_dir);
    assert!(before.len() >= 3, "a multi-file corpus: {before:?}");

    let hold = Hold::at_progress();

    let started = match h
        .schema_admin
        .schema_repin(
            "status",
            "VARCHAR",
            None,
            false,
            false,
            RepinCeilings::default(),
        )
        .await
        .expect("execute")
    {
        RepinStart::Started(job) => job,
        other => panic!("expected started, got {other:?}"),
    };
    await_barrier(
        &TEST_PROGRESS_PUBLISHED,
        "the build never published progress",
    )
    .await;

    assert!(trawl_server::repin::shadow_root(&h.data_dir).exists());
    assert!(trawl_server::repin::marker_path(&h.data_dir).exists());

    let receipt = match h.schema_admin.schema_repin_cancel().await.expect("cancel") {
        RepinCancel::Cancelling(body) => body,
        other => panic!("expected an accepted cancel, got {other:?}"),
    };
    assert!(
        receipt
            .detail
            .contains(trawl_server::repin::CANCEL_LATENCY_CONTRACT),
        "the 202 tells the operator what accepted means: {}",
        receipt.detail
    );
    assert_eq!(
        receipt.job.as_ref().map(|j| j.id),
        Some(started.id),
        "the receipt names the job it stopped"
    );
    let requested_at = h.await_cancel_request(started.id).await;

    match h
        .schema_admin
        .schema_repin_cancel()
        .await
        .expect("second cancel")
    {
        RepinCancel::Cancelling(_) => {}
        other => panic!("a repeat request is accepted, got {other:?}"),
    }

    hold.release();
    let done = h.wait_terminal(started.id).await;
    assert_eq!(done.status, "cancelled", "error: {:?}", done.error);
    assert_eq!(done.cancelled_by.as_deref(), Some("schema-admin-key"));
    assert!(done.cancel_requested_at.is_some());
    let error = done.error.clone().expect("a cancelled row explains itself");
    assert!(
        error.contains("cancelled by schema-admin-key during build")
            && error.contains("never touched"),
        "{error}"
    );
    assert_eq!(
        h.repin_row(started.id).await.cancel_requested_at,
        Some(requested_at),
        "neither the repeat request nor the effect site may restamp the first"
    );

    assert!(!trawl_server::repin::shadow_root(&h.data_dir).exists());
    assert!(!trawl_server::repin::aside_root(&h.data_dir).exists());
    assert!(!trawl_server::repin::marker_path(&h.data_dir).exists());

    assert_eq!(corpus_digest(&h.data_dir), before);
    assert_eq!(h.pinned_type("status").await, "BIGINT", "the pin stands");
    assert_eq!(h.count("last=1h | stats count()").await, 6);
    assert_eq!(h.count("status>=400 last=1h | stats count()").await, 3);

    // The terminal row lands before the outcome is counted, and the job
    // generation ends only after the count, so the scrape waits for it.
    h.await_jobs_idle().await;
    let scrape = scrape_metrics(&h.server.url).await;
    assert_eq!(
        counter_value(
            &scrape,
            trawl_server::metrics::CATALOG_REPIN_JOBS_TOTAL,
            "outcome=\"cancelled\""
        ),
        Some(1.0),
        "one cancelled job, one increment: {scrape}"
    );
    assert_eq!(
        counter_value(
            &scrape,
            trawl_server::metrics::CATALOG_REPIN_JOBS_TOTAL,
            "outcome=\"succeeded\""
        ),
        None,
        "a cancelled job is not a completed one"
    );

    let suppressed = h.await_retention_gauge().await;
    assert!(
        suppressed.abs() < f64::EPSILON,
        "a cancelled job must not leave retention suppressed: {suppressed}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_during_the_scan_stops_before_any_plan_is_published() {
    for force in [false, true] {
        // The dry run's own request must still be waiting when the cancel
        // lands, so its answer bound is the hang guard: under the default
        // 10 s, a slow scan, cancel and release would answer 202 instead.
        let h = hang_guard_harness().await;
        let services = ["api", "web", "worker", "edge"];
        for svc in services {
            h.ingest_and_compact(&[event(svc, &json!({"status": 200}))])
                .await;
        }

        let hold = Hold::in_scan();

        let client = h.schema_admin.clone();
        let dry = tokio::spawn(async move {
            client
                .schema_repin(
                    "status",
                    "VARCHAR",
                    None,
                    true,
                    force,
                    RepinCeilings::default(),
                )
                .await
        });
        await_barrier(&TEST_SCAN_HELD, "the scan never reached a file boundary").await;

        assert!(!trawl_server::repin::marker_path(&h.data_dir).exists());
        assert!(!trawl_server::repin::shadow_root(&h.data_dir).exists());

        match h.schema_admin.schema_repin_cancel().await.expect("cancel") {
            RepinCancel::Cancelling(_) => {}
            other => panic!("expected an accepted cancel, got {other:?}"),
        }
        hold.release();

        let report = match dry
            .await
            .expect("join")
            .expect("a cancelled dry run answers with its row, not an error")
        {
            // The 200 is shared with the dry-run report, and the client tells
            // the two apart by the row's own status, so a cancelled job can
            // never be printed as a plan.
            RepinStart::Cancelled(job) => job,
            other => panic!("expected the cancelled row, got {other:?}"),
        };
        assert_eq!(report.status, "cancelled");
        assert_eq!(report.force, force);
        assert_eq!(report.requires_force, None);
        assert_eq!(report.requires_force_reason, None);
        assert_eq!(report.accepted_max_nulled_rows, None);
        assert_eq!(report.accepted_max_ambiguous_rows, None);
        assert_eq!(report.cancelled_by.as_deref(), Some("schema-admin-key"));
        let error = report
            .error
            .clone()
            .expect("a cancelled row explains itself");
        assert!(error.contains("during scan"), "{error}");

        let row = h.repin_row(report.id).await;
        assert_eq!(row.planned_at, None, "no partial plan may be published");
        assert_eq!(report.files_total, 0);
        assert!(
            report.files_total < u64::try_from(services.len()).unwrap(),
            "the scan stopped short of the corpus"
        );

        assert!(!trawl_server::repin::marker_path(&h.data_dir).exists());
        assert!(!trawl_server::repin::shadow_root(&h.data_dir).exists());
        assert!(!trawl_server::repin::aside_root(&h.data_dir).exists());
        assert_eq!(h.pinned_type("status").await, "BIGINT");
    }
}

// The answer-bound tests pause tokio's clock after boot. The server's tasks
// share the test's one `current_thread` runtime, so the server's `Deadline`
// reads the paused clock. Every wait from the pause on goes through the
// `test_timing` yield driver, which keeps the runtime busy, so the clock
// never auto-advances. An idle tokio sleep or timeout would let it jump.

/// The `[server] timeout_secs` of the answer-bound tests, which is also
/// the trigger's answer bound.
const ANSWER_BOUND: Duration = Duration::from_secs(1);

async fn answer_bound_harness() -> Harness {
    harness_with_query_timeout(ANSWER_BOUND.as_secs()).await
}

async fn frozen<F: std::future::Future>(what: &str, fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    until_polling(what, &mut fut, || false)
        .await
        .expect("until_polling returns only once the future completes")
}

async fn frozen_until<T, F>(what: &str, mut probe: impl FnMut() -> F) -> T
where
    F: std::future::Future<Output = Option<T>>,
{
    let start = std::time::Instant::now();
    loop {
        if let Some(found) = frozen(what, probe()).await {
            return found;
        }
        assert!(
            start.elapsed() < HANG_GUARD,
            "timed out after {HANG_GUARD:?} waiting for: {what}"
        );
        tokio::task::yield_now().await;
    }
}

/// The server's count of regular requests in progress, read off a
/// `/metrics` scrape. The scrape itself rides the probe allowance, so it
/// is not in this count. A request keeps its count until its response head
/// is produced (ADR-0054), so with the repin trigger the only regular
/// request in flight, 1 means it has not answered and 0 that it has.
async fn regular_requests_in_progress(h: &Harness) -> f64 {
    let scrape = frozen("a metrics scrape", scrape_metrics(&h.server.url)).await;
    counter_value(
        &scrape,
        trawl_server::metrics::REQUESTS_IN_PROGRESS,
        r#"allowance="regular""#,
    )
    .expect("the regular in-progress gauge is published")
}

const ANSWERED_ON_A_FROZEN_CLOCK: &str =
    "the trigger answered on a frozen clock, before its bound passed";

/// The claim hands its row to the waiting request before the scan starts,
/// so the held scan means the claim landed and the request holds its row.
async fn drive_to_the_held_scan<F>(h: &Harness, request: &mut F) -> RepinJob
where
    F: std::future::Future + Unpin,
    F::Output: std::fmt::Debug,
{
    if let Some(early) = until_polling("the scan reaches its first file", request, || {
        TEST_SCAN_HELD.load(Ordering::SeqCst)
    })
    .await
    {
        panic!("{ANSWERED_ON_A_FROZEN_CLOCK}: {early:?}");
    }
    let claimed = frozen("the claimed row", h.server.state.storage.repin.running())
        .await
        .expect("read")
        .expect("the claim landed before the scan");
    assert!(claimed.dry_run, "{claimed:?}");
    claimed
}

async fn assert_unanswered_on_the_frozen_clock<F>(h: &Harness, request: &mut F)
where
    F: std::future::Future + Unpin,
    F::Output: std::fmt::Debug,
{
    let url = h.server.url.clone();
    let scrape = tokio::spawn(async move { scrape_metrics(&url).await });
    if let Some(early) =
        until_polling("a metrics round trip", request, || scrape.is_finished()).await
    {
        panic!("{ANSWERED_ON_A_FROZEN_CLOCK}: {early:?}");
    }
    let scrape = frozen("the scrape's join", scrape)
        .await
        .expect("the scrape task");
    assert_eq!(
        counter_value(
            &scrape,
            trawl_server::metrics::REQUESTS_IN_PROGRESS,
            r#"allowance="regular""#,
        ),
        Some(1.0),
        "{ANSWERED_ON_A_FROZEN_CLOCK}: the server no longer counts the trigger in progress"
    );
}

async fn pass_the_answer_bound() {
    tokio::time::advance(ANSWER_BOUND + Duration::from_millis(1)).await;
}

/// The scan is still held, so the answer is the 202 with the claimed row,
/// and the hold may be released without the scan deciding inside the bound.
async fn await_the_late_answer(h: &Harness) {
    frozen_until("the trigger answers past its bound", move || async move {
        (regular_requests_in_progress(h).await == 0.0).then_some(())
    })
    .await;
}

/// The CLI's status reads run a client timeout on tokio's clock, and nothing
/// publishes when a read is in flight, so stepping a frozen clock could time
/// out a pending read on a correct build. Every precondition is already a
/// fact here, so the clock resumes.
async fn follow_the_cli_on_real_time<F: std::future::Future + Unpin>(cli: &mut F) -> F::Output {
    tokio::time::resume();
    until_polling("the CLI follows its job", cli, || false)
        .await
        .expect("the CLI ends before an unreachable condition")
}

impl Harness {
    async fn frozen_terminal(&self, id: i64) -> RepinJob {
        frozen_until("the job leaves running", move || async move {
            let row = self.repin_row(id).await;
            (row.status != RepinJobStatus::Running).then_some(row)
        })
        .await
    }
}

#[tokio::test]
async fn a_scan_past_the_answer_bound_answers_202_and_reports_through_status() {
    let h = answer_bound_harness().await;
    for svc in ["api", "web"] {
        h.ingest_and_compact(&[event(svc, &json!({"status": 200}))])
            .await;
    }

    tokio::time::pause();
    let hold = Hold::in_scan();
    let mut request = std::pin::pin!(h.schema_admin.schema_repin(
        "status",
        "VARCHAR",
        None,
        true,
        false,
        RepinCeilings::default(),
    ));
    let claimed = drive_to_the_held_scan(&h, &mut request).await;
    assert_unanswered_on_the_frozen_clock(&h, &mut request).await;

    pass_the_answer_bound().await;
    let answered = frozen("the trigger answers past its bound", &mut request)
        .await
        .expect("an admitted dry run answers with its row");
    assert!(
        TEST_SCAN_HELD.load(Ordering::SeqCst),
        "the hold is unreleased, so the answer came while the scan was held"
    );
    let accepted = match answered {
        RepinStart::Started(job) => job,
        other => panic!("expected 202 with the running row, got {other:?}"),
    };
    assert_eq!(accepted.id, claimed.id);
    assert_eq!(accepted.status, "running");
    assert!(accepted.dry_run);
    assert_eq!(
        accepted.requires_force, None,
        "no plan is recorded yet, so there is no verdict: {accepted:?}"
    );

    hold.release();
    let terminal = h.frozen_terminal(accepted.id).await;
    assert_eq!(terminal.status, RepinJobStatus::Succeeded, "{terminal:?}");
    let report = frozen("the status read", h.schema_admin.schema_repin_status())
        .await
        .expect("status")
        .job
        .expect("the claimed job stays on the status route");
    assert_eq!(
        report.id, accepted.id,
        "no other job can take the slot here"
    );
    assert_eq!(report.status, "succeeded", "{report:?}");
    assert_eq!(report.files_total, 2, "{report:?}");
    assert_eq!(report.requires_force, Some(false), "{report:?}");
    assert_eq!(
        frozen("the pinned type", h.pinned_type("status")).await,
        "BIGINT"
    );
}

#[tokio::test]
async fn the_cli_follows_a_dry_run_answered_past_the_bound_to_its_report() {
    use trawl_cli::cli::{ConnectionParams, OutputFormat};
    use trawl_cli::schema::{RepinFlags, run_repin};

    let h = answer_bound_harness().await;
    for svc in ["api", "web"] {
        h.ingest_and_compact(&[event(svc, &json!({"status": 200}))])
            .await;
    }

    tokio::time::pause();
    let hold = Hold::in_scan();
    let conn = ConnectionParams {
        url: h.server.url.clone(),
        token: h.server.schema_admin_token.clone(),
        trust: trawl_client::TlsTrust::AcceptInvalid,
    };
    let flags = RepinFlags {
        dialect: None,
        dry_run: true,
        force: false,
        yes: false,
        wait: false,
        max_nulled_rows: None,
        max_ambiguous_rows: None,
    };
    let mut out = Vec::new();
    let result = {
        let mut cli = std::pin::pin!(run_repin(
            &mut out,
            conn,
            "status",
            "VARCHAR",
            flags,
            Some(OutputFormat::Table),
        ));
        let claimed = drive_to_the_held_scan(&h, &mut cli).await;
        assert_unanswered_on_the_frozen_clock(&h, &mut cli).await;

        pass_the_answer_bound().await;
        await_the_late_answer(&h).await;
        let running = frozen("the status read", h.schema_admin.schema_repin_status())
            .await
            .expect("status")
            .job
            .expect("the claimed job is on the status route");
        assert_eq!(running.id, claimed.id);
        assert_eq!(running.status, "running", "{running:?}");
        hold.release();
        h.frozen_terminal(claimed.id).await;

        follow_the_cli_on_real_time(&mut cli).await
    };
    let text = String::from_utf8(out).unwrap();
    result.unwrap_or_else(|e| panic!("the dry run reports and exits 0: {e:?}\n{text}"));
    assert!(
        text.contains("repin status: dry run"),
        "a followed dry run prints as a report, not as started: {text}"
    );
    assert!(text.contains("succeeded"), "{text}");
}

async fn status_by_id(h: &Harness, token: &str, id: i64) -> (u16, serde_json::Value) {
    status_with_query(h, token, &format!("id={id}")).await
}

async fn status_with_query(h: &Harness, token: &str, query: &str) -> (u16, serde_json::Value) {
    let resp = common::harness_client_builder()
        .build()
        .unwrap()
        .get(format!(
            "{}/api/v1/schema/repin/status?{query}",
            h.server.url
        ))
        .bearer_auth(token)
        .send()
        .await
        .expect("status by id");
    let status = resp.status().as_u16();
    (status, resp.json().await.expect("a JSON body"))
}

async fn schema_write_only_token(h: &Harness) -> String {
    seed_schema_write_only_role(h).await;
    mint_schema_write_only_key(h, "schema-writer").await
}

/// Seed it once per harness.
async fn seed_schema_write_only_role(h: &Harness) {
    fleet_auth::KeyStore::from_pool(h.server.fleet_pool.clone())
        .create_role(
            "trawl-schema-writer",
            None,
            &common::trawl_perms(&["schema_write"]),
        )
        .await
        .expect("seed a write-only role");
}

async fn mint_schema_write_only_key(h: &Harness, name: &str) -> String {
    fleet_auth::KeyStore::from_pool(h.server.fleet_pool.clone())
        .create_key(
            name,
            fleet_auth::PrincipalKind::Service,
            &common::roles(&["trawl-schema-writer"]),
            None,
        )
        .await
        .expect("mint a write-only key")
        .plaintext_token
        .to_string()
}

async fn held_dry_run_past_the_bound(h: &Harness, client: &HttpClient) -> (RepinJobResponse, Hold) {
    let hold = Hold::in_scan();
    let mut request = std::pin::pin!(client.schema_repin(
        "status",
        "VARCHAR",
        None,
        true,
        false,
        RepinCeilings::default(),
    ));
    let claimed = drive_to_the_held_scan(h, &mut request).await;
    assert_unanswered_on_the_frozen_clock(h, &mut request).await;

    pass_the_answer_bound().await;
    let answered = frozen("the trigger answers past its bound", &mut request)
        .await
        .expect("an admitted dry run answers with its row");
    assert!(
        TEST_SCAN_HELD.load(Ordering::SeqCst),
        "the hold is unreleased, so the answer came while the scan was held"
    );
    match answered {
        RepinStart::Started(job) => {
            assert_eq!(job.id, claimed.id);
            (job, hold)
        }
        other => panic!("expected 202 with the running row, got {other:?}"),
    }
}

#[tokio::test]
async fn a_dry_run_answered_202_is_read_by_its_id_after_another_job_takes_the_slot() {
    let h = answer_bound_harness().await;
    for svc in ["api", "web"] {
        h.ingest_and_compact(&[event(svc, &json!({"status": 200}))])
            .await;
    }
    let token = h.server.schema_admin_token.clone();

    tokio::time::pause();
    let (first, first_hold) = held_dry_run_past_the_bound(&h, &h.schema_admin).await;
    first_hold.release();
    let finished = h.frozen_terminal(first.id).await;
    assert_eq!(finished.status, RepinJobStatus::Succeeded, "{finished:?}");

    // Another job takes the freed slot and holds it, once the first job's
    // task gives up the permit.
    h.wait_idle().await;
    let (second, second_hold) = held_dry_run_past_the_bound(&h, &h.schema_admin).await;
    assert_ne!(second.id, first.id);
    let unscoped = frozen(
        "the unscoped status read",
        h.schema_admin.schema_repin_status(),
    )
    .await
    .expect("status")
    .job
    .expect("a job is running");
    assert_eq!(
        unscoped.id, second.id,
        "the unscoped read names the newer job"
    );

    let (status, body) = frozen("the read by id", status_by_id(&h, &token, first.id)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["job"]["id"], first.id,
        "the read by id names the followed job, not the one now in the slot: {body}"
    );
    assert_eq!(body["job"]["status"], "succeeded", "{body}");
    assert_eq!(
        body["job"]["files_total"], 2,
        "the report rides the row: {body}"
    );

    let (status, body) = frozen(
        "the read of an unknown id",
        status_by_id(&h, &token, second.id + 1000),
    )
    .await;
    assert_eq!(status, 404, "an unknown id is not found: {body}");
    assert_eq!(body["error"]["code"], "not_found", "{body}");

    let (status, body) = frozen(
        "the read of a malformed id",
        status_with_query(&h, &token, "id=seven"),
    )
    .await;
    assert_eq!(
        status, 400,
        "an id that is not an integer is refused: {body}"
    );
    assert_eq!(body["error"]["code"], "bad_request", "{body}");

    second_hold.release();
    h.frozen_terminal(second.id).await;
}

#[tokio::test]
async fn a_schema_write_only_key_follows_its_dry_run_by_id() {
    let h = answer_bound_harness().await;
    for svc in ["api", "web"] {
        h.ingest_and_compact(&[event(svc, &json!({"status": 200}))])
            .await;
    }
    let token = schema_write_only_token(&h).await;
    let writer = HttpClient::new_insecure(&h.server.url, &token).unwrap();

    tokio::time::pause();
    let (accepted, hold) = held_dry_run_past_the_bound(&h, &writer).await;
    hold.release();
    h.frozen_terminal(accepted.id).await;
    let (status, report) = frozen("the read by id", status_by_id(&h, &token, accepted.id)).await;
    assert_eq!(
        status, 200,
        "a write-only key reads its own job by id: {report}"
    );
    assert_eq!(report["job"]["id"], accepted.id, "{report}");
    assert_eq!(report["job"]["status"], "succeeded", "{report}");
    assert_eq!(report["job"]["files_total"], 2, "{report}");

    match frozen("the unscoped status read", writer.schema_repin_status()).await {
        Err(trawl_client::ClientError::Server { status, .. }) => assert_eq!(status, 403),
        other => panic!("the unscoped read still needs schema_read: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_schema_write_only_key_reads_only_the_jobs_it_requested() {
    let h = harness().await;
    for svc in ["api", "web"] {
        h.ingest_and_compact(&[event(svc, &json!({"status": 200}))])
            .await;
    }
    // A job from before either write-only key existed, requested by a key
    // that holds `schema_read` as well.
    let historical = dry_run(&h.schema_admin, "status", "VARCHAR", None, false).await;
    seed_schema_write_only_role(&h).await;
    let token = mint_schema_write_only_key(&h, "writer-a").await;
    let other_token = mint_schema_write_only_key(&h, "writer-b").await;
    let writer = HttpClient::new_insecure(&h.server.url, &token).unwrap();
    let other_writer = HttpClient::new_insecure(&h.server.url, &other_token).unwrap();
    let own = dry_run(&writer, "status", "VARCHAR", None, false).await;
    let other = dry_run(&other_writer, "status", "VARCHAR", None, false).await;
    let unknown = own.id.max(other.id).max(historical.id) + 1000;

    let (status, body) = status_by_id(&h, &token, own.id).await;
    assert_eq!(status, 200, "a write-only key reads its own job: {body}");
    assert_eq!(body["job"]["id"], own.id, "{body}");
    assert_eq!(body["job"]["requested_by"], "writer-a", "{body}");

    let mut refusals = Vec::new();
    for (what, id) in [
        ("another key's job", other.id),
        ("a job from before the key existed", historical.id),
        ("an id the store never issued", unknown),
    ] {
        let (status, body) = status_by_id(&h, &token, id).await;
        assert_eq!(status, 404, "{what} (id {id}) reads as not found: {body}");
        assert_eq!(body["error"]["code"], "not_found", "{what}: {body}");
        refusals.push(body);
    }
    assert!(
        refusals.windows(2).all(|pair| pair[0] == pair[1]),
        "the refusal body must not say whether the job exists: {refusals:?}"
    );

    for id in [own.id, other.id, historical.id] {
        let (status, body) = status_by_id(&h, &h.server.reader_token, id).await;
        assert_eq!(status, 200, "a schema_read key reads job {id}: {body}");
        assert_eq!(body["job"]["id"], id, "{body}");
    }

    match writer.schema_repin_status().await {
        Err(trawl_client::ClientError::Server { status, .. }) => assert_eq!(status, 403),
        other => panic!("the unscoped read still needs schema_read: {other:?}"),
    }
}

#[tokio::test]
async fn the_cli_follows_a_late_dry_run_with_a_schema_write_only_key() {
    use trawl_cli::cli::{ConnectionParams, OutputFormat};
    use trawl_cli::schema::{RepinFlags, run_repin};

    let h = answer_bound_harness().await;
    for svc in ["api", "web"] {
        h.ingest_and_compact(&[event(svc, &json!({"status": 200}))])
            .await;
    }
    let token = schema_write_only_token(&h).await;

    tokio::time::pause();
    let hold = Hold::in_scan();
    let conn = ConnectionParams {
        url: h.server.url.clone(),
        token,
        trust: trawl_client::TlsTrust::AcceptInvalid,
    };
    let flags = RepinFlags {
        dialect: None,
        dry_run: true,
        force: false,
        yes: false,
        wait: false,
        max_nulled_rows: None,
        max_ambiguous_rows: None,
    };
    let mut out = Vec::new();
    let result = {
        let mut cli = std::pin::pin!(run_repin(
            &mut out,
            conn,
            "status",
            "VARCHAR",
            flags,
            Some(OutputFormat::Table),
        ));
        let claimed = drive_to_the_held_scan(&h, &mut cli).await;
        assert_unanswered_on_the_frozen_clock(&h, &mut cli).await;

        pass_the_answer_bound().await;
        await_the_late_answer(&h).await;
        hold.release();
        h.frozen_terminal(claimed.id).await;

        follow_the_cli_on_real_time(&mut cli).await
    };
    let text = String::from_utf8(out).unwrap();
    result.unwrap_or_else(|e| {
        panic!("a write-only key's dry run reports and exits 0: {e:?}\n{text}")
    });
    assert!(text.contains("repin status: dry run"), "{text}");
    assert!(text.contains("succeeded"), "{text}");
}

#[tokio::test]
async fn the_cli_prints_its_own_report_after_another_job_takes_the_slot() {
    use trawl_cli::cli::{ConnectionParams, OutputFormat};
    use trawl_cli::schema::{RepinFlags, run_repin};

    let h = answer_bound_harness().await;
    for svc in ["api", "web"] {
        h.ingest_and_compact(&[event(svc, &json!({"status": 200}))])
            .await;
    }

    tokio::time::pause();
    let first_hold = Hold::in_scan();
    let conn = ConnectionParams {
        url: h.server.url.clone(),
        token: h.server.schema_admin_token.clone(),
        trust: trawl_client::TlsTrust::AcceptInvalid,
    };
    let flags = RepinFlags {
        dialect: None,
        dry_run: true,
        force: false,
        yes: false,
        wait: false,
        max_nulled_rows: None,
        max_ambiguous_rows: None,
    };
    let store = h.server.state.storage.repin.clone();
    let mut out = Vec::new();
    let (result, first_id, second_id, second, second_hold) = {
        let mut cli = std::pin::pin!(run_repin(
            &mut out,
            conn,
            "status",
            "VARCHAR",
            flags,
            Some(OutputFormat::Table),
        ));
        let first = drive_to_the_held_scan(&h, &mut cli).await;
        assert_unanswered_on_the_frozen_clock(&h, &mut cli).await;

        pass_the_answer_bound().await;
        await_the_late_answer(&h).await;
        // The CLI holds its 202 now and polls only once the clock moves by
        // its poll interval, which nothing below does.
        first_hold.release();
        let terminal = h.frozen_terminal(first.id).await;
        assert_eq!(terminal.status, RepinJobStatus::Succeeded, "{terminal:?}");
        assert!(
            frozen("the running slot", store.running())
                .await
                .expect("read")
                .is_none(),
            "the slot is free once the first job is terminal"
        );
        h.wait_idle().await;

        // Armed only once the first job is gone and its guard consumed, so
        // the first guard cannot release this hold.
        let second_hold = Hold::in_scan();
        let admin = h.schema_admin.clone();
        let second = tokio::spawn(async move {
            admin
                .schema_repin(
                    "status",
                    "VARCHAR",
                    None,
                    true,
                    false,
                    RepinCeilings::default(),
                )
                .await
        });
        // Waits for: the second scan's arrival at its first file, which
        // follows its claim. The CLI is polled throughout and cannot finish:
        // its poll sleep has not fired.
        if let Some(early) =
            until_polling("the second job reaches its first file", &mut cli, || {
                TEST_SCAN_HELD.load(Ordering::SeqCst)
            })
            .await
        {
            panic!("the CLI read its job before the clock moved its poll: {early:?}");
        }
        let running = frozen("the running slot", store.running())
            .await
            .expect("read")
            .expect("the second job runs");
        assert_ne!(running.id, first.id, "a newer job holds the slot");

        // The second trigger answers past its own bound before the CLI is
        // stepped, so the stepping below sees only the CLI's reads in
        // flight. The CLI registered its poll sleep at the first 202, one
        // full interval before this advance ends, so this does not wake it.
        pass_the_answer_bound().await;
        await_the_late_answer(&h).await;

        let result = follow_the_cli_on_real_time(&mut cli).await;
        (result, first.id, running.id, second, second_hold)
    };
    let text = String::from_utf8(out).unwrap();
    // The clock passed the second trigger's bound with its scan held, and
    // its response head was produced before the CLI was stepped, so its
    // answer is the 202.
    second_hold.release();
    match frozen("the second trigger answers", second)
        .await
        .expect("the second trigger's task")
        .expect("an admitted dry run answers with its row")
    {
        RepinStart::Started(job) => assert_eq!(job.id, second_id),
        other => panic!("expected 202 with the second job's row, got {other:?}"),
    }
    h.frozen_terminal(second_id).await;
    result.unwrap_or_else(|e| panic!("the CLI prints its own job's report: {e:?}\n{text}"));
    assert!(text.contains("repin status: dry run"), "{text}");
    assert!(text.contains("succeeded"), "{text}");
    assert!(
        text.contains(&first_id.to_string()),
        "the report is the first job's: {text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_claim_still_pending_at_the_bound_answers_503_and_its_late_job_never_scans() {
    use sqlx::Connection as _;

    let h = harness_with_query_timeout(1).await;
    for svc in ["api", "web"] {
        h.ingest_and_compact(&[event(svc, &json!({"status": 200}))])
            .await;
    }
    let mut blocker = sqlx::postgres::PgConnection::connect(&h.server.app_db_url)
        .await
        .expect("connect app db");
    let mut lock = blocker.begin().await.expect("begin");
    // SHARE conflicts with the claim's INSERT and with no read.
    sqlx::query("LOCK TABLE repin_jobs IN SHARE MODE")
        .execute(&mut *lock)
        .await
        .expect("hold the claim");

    let started = std::time::Instant::now();
    let request = async {
        let answered = h
            .schema_admin
            .schema_repin(
                "status",
                "VARCHAR",
                None,
                true,
                false,
                RepinCeilings::default(),
            )
            .await;
        (answered, started.elapsed())
    };
    let hold = async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        lock.rollback().await.expect("release the claim");
    };
    let ((answered, elapsed), ()) = tokio::join!(request, hold);
    assert!(
        elapsed < Duration::from_millis(2500),
        "the request answered {elapsed:?} after it started, past its 1 s bound"
    );
    match answered {
        Err(trawl_client::ClientError::Server { status, error, .. }) => {
            assert_eq!(status, 503, "{error:?}");
            assert_eq!(error.code, trawl_api::ErrorCode::ServiceUnavailable);
        }
        other => panic!("a claim pending at the bound answers 503: {other:?}"),
    }

    let store = &h.server.state.storage.repin;
    let mut late = None;
    for _ in 0..200 {
        if let Some(job) = store.latest().await.expect("read")
            && job.status != RepinJobStatus::Running
        {
            late = Some(job);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let late = late.expect("the late claim lands and ends");
    assert_eq!(late.status, RepinJobStatus::Blocked, "{late:?}");
    assert!(
        late.planned_at.is_none(),
        "the late job never scanned: {late:?}"
    );
    assert_eq!(h.pinned_type("status").await, "BIGINT");

    h.wait_idle().await;
    match h
        .schema_admin
        .schema_repin(
            "status",
            "VARCHAR",
            None,
            true,
            false,
            RepinCeilings::default(),
        )
        .await
        .expect("the slot is free again")
    {
        RepinStart::Report(job) => assert_eq!(job.status, "succeeded"),
        other => panic!("a fresh dry run reports: {other:?}"),
    }
}

#[tokio::test]
async fn a_late_answer_reads_no_job_row_after_its_bound() {
    use sqlx::Connection as _;

    let h = answer_bound_harness().await;
    for svc in ["api", "web"] {
        h.ingest_and_compact(&[event(svc, &json!({"status": 200}))])
            .await;
    }

    tokio::time::pause();
    let hold = Hold::in_scan();
    let mut request = std::pin::pin!(h.schema_admin.schema_repin(
        "status",
        "VARCHAR",
        None,
        true,
        false,
        RepinCeilings::default(),
    ));
    let claimed = drive_to_the_held_scan(&h, &mut request).await;

    let mut conn = frozen(
        "connect the app db",
        sqlx::postgres::PgConnection::connect(&h.server.app_db_url),
    )
    .await
    .expect("connect app db");
    let mut lock = frozen("begin", conn.begin()).await.expect("begin");
    frozen(
        "the table lock",
        sqlx::query("LOCK TABLE repin_jobs IN ACCESS EXCLUSIVE MODE").execute(&mut *lock),
    )
    .await
    .expect("hold every read");

    assert_unanswered_on_the_frozen_clock(&h, &mut request).await;

    pass_the_answer_bound().await;
    let answered = frozen("the late answer, with the job rows locked", &mut request).await;
    frozen("release the reads", lock.rollback())
        .await
        .expect("release the reads");
    let accepted = match answered.expect("the late answer is a 202") {
        RepinStart::Started(job) => job,
        other => panic!("expected 202 with the claimed row, got {other:?}"),
    };
    assert_eq!(accepted.id, claimed.id);
    assert_eq!(accepted.status, "running");
    assert!(accepted.dry_run);
    assert_eq!(accepted.field, "status");

    hold.release();
    let report = h.frozen_terminal(accepted.id).await;
    assert_eq!(report.status, RepinJobStatus::Succeeded, "{report:?}");
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one held claim, sixteen requests, and the job they leave
async fn requests_dropped_before_their_claim_leave_one_claim_in_flight() {
    use sqlx::Connection as _;
    use trawl_server::error::ServerError;

    let h = harness().await;
    for svc in ["api", "web"] {
        h.ingest_and_compact(&[event(svc, &json!({"status": 200}))])
            .await;
    }
    let engine = h.engine();
    let mut blocker = sqlx::postgres::PgConnection::connect(&h.server.app_db_url)
        .await
        .expect("connect app db");
    let mut lock = blocker.begin().await.expect("begin");
    sqlx::query("LOCK TABLE repin_jobs IN SHARE MODE")
        .execute(&mut *lock)
        .await
        .expect("hold the claim");

    let requester = op();
    let request = || {
        engine.start_within(
            Duration::from_secs(60),
            "status",
            "VARCHAR",
            None,
            true,
            false,
            RequestedCeilings::default(),
            Some(&requester),
        )
    };
    // Waits for: the first request's task to take the permit. Its claim
    // then waits on the lock, and the future is dropped as hyper drops a
    // handler whose connection went away.
    let mut first = Box::pin(request());
    if let Some(early) = until_polling("the first request takes the permit", &mut first, || {
        !engine.repin_idle()
    })
    .await
    {
        panic!("the claim is held, so the first request cannot answer yet: {early:?}");
    }
    drop(first);
    for _ in 0..15 {
        match request().await {
            Err(ServerError::Conflict(message)) => assert_eq!(message, PERMIT_HELD),
            other => panic!("a request behind the permit is refused at once: {other:?}"),
        }
    }

    let mut observer = sqlx::postgres::PgConnection::connect(&h.server.app_db_url)
        .await
        .expect("connect app db");
    let mut most = 0_i64;
    for _ in 0..20 {
        // Every backend in this database waiting on any lock: the first
        // claim waits on the table, and any other would wait on the
        // catalog advisory lock that first claim holds.
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(DISTINCT pid) FROM pg_locks
             WHERE NOT granted
               AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
        )
        .fetch_one(&mut observer)
        .await
        .expect("count waiting claims");
        most = most.max(waiting);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    lock.rollback().await.expect("release the claim");
    assert_eq!(
        most, 1,
        "sixteen requests must leave one claim in flight, not one each"
    );

    let store = &h.server.state.storage.repin;
    let mut ended = None;
    for _ in 0..200 {
        if let Some(job) = store.latest().await.expect("read")
            && job.status != RepinJobStatus::Running
        {
            ended = Some(job);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let ended = ended.expect("the one claim lands and ends");
    assert_eq!(ended.status, RepinJobStatus::Blocked, "{ended:?}");
    assert!(ended.planned_at.is_none(), "it never scanned: {ended:?}");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM repin_jobs")
        .fetch_one(&mut observer)
        .await
        .expect("count jobs");
    assert_eq!(rows, 1, "the sixteen requests claimed one job between them");

    h.wait_idle().await;
    match engine
        .start_within(
            Duration::from_secs(60),
            "status",
            "VARCHAR",
            None,
            true,
            false,
            RequestedCeilings::default(),
            Some(&op()),
        )
        .await
        .expect("the slot is free again")
    {
        trawl_server::repin::BoundedStart::Decided(StartOutcome::DryRun(job)) => {
            assert_eq!(job.status, RepinJobStatus::Succeeded);
        }
        other => panic!("a fresh dry run reports: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_scan_panic_reaches_the_request_as_a_panic() {
    use trawl_server::error::ServerError;
    use trawl_server::repin::engine::TEST_PANIC_IN_SCAN;

    const SENTINEL: &str = "zz_repin_scan_payload_sentinel";
    let h = harness().await;
    h.ingest_and_compact(&[event("api", &json!({"status": 200}))])
        .await;
    let engine = h.engine();

    // Both halves of the ladder share the scan. The second pass also
    // proves the first freed the running slot and the repin permit.
    for dry_run in [true, false] {
        TEST_PANIC_IN_SCAN.store(true, Ordering::SeqCst);
        let err = engine
            .start(
                "status",
                "VARCHAR",
                None,
                dry_run,
                false,
                RequestedCeilings::default(),
                Some(&op()),
            )
            .await
            .expect_err("a panicked scan fails the request");
        assert!(
            matches!(err, ServerError::Panicked("repin scan")),
            "dry_run={dry_run}: got {err:?}"
        );
        assert_eq!(err.error_class(), "panic");
        assert!(!format!("{err} {err:?}").contains(SENTINEL));

        let job = engine.store().latest().await.unwrap().unwrap();
        assert_eq!(job.status, RepinJobStatus::Failed);
        assert_eq!(job.error.as_deref(), Some("repin scan task panicked"));
        assert_eq!(job.planned_at, None, "a panicked scan publishes no plan");
        assert!(!trawl_server::repin::marker_path(&h.data_dir).exists());
        assert!(!trawl_server::repin::shadow_root(&h.data_dir).exists());
    }

    TEST_PANIC_IN_SCAN.store(true, Ordering::SeqCst);
    match h
        .schema_admin
        .schema_repin(
            "status",
            "VARCHAR",
            None,
            true,
            false,
            RepinCeilings::default(),
        )
        .await
    {
        Err(trawl_client::ClientError::Server { status, error, .. }) => {
            assert_eq!(status, 500);
            assert_eq!(error.message, "internal server error");
            assert!(error.details.is_empty());
            assert!(!format!("{error:?}").contains(SENTINEL));
        }
        other => panic!("expected a 500, got {other:?}"),
    }
    let job = engine.store().latest().await.unwrap().unwrap();
    assert_eq!(job.error.as_deref(), Some("repin scan task panicked"));
    assert_eq!(h.pinned_type("status").await, "BIGINT");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_past_the_point_of_no_return_is_refused_and_the_job_completes() {
    let h = hang_guard_harness().await;
    h.ingest_and_compact(&[
        event("api", &json!({"status": 200})),
        event("api", &json!({"status": 404})),
    ])
    .await;
    assert_eq!(h.pinned_type("status").await, "BIGINT");

    let hold = Hold::past_no_return();

    let started = match h
        .schema_admin
        .schema_repin(
            "status",
            "VARCHAR",
            None,
            false,
            false,
            RepinCeilings::default(),
        )
        .await
        .expect("execute")
    {
        RepinStart::Started(job) => job,
        other => panic!("expected started, got {other:?}"),
    };
    await_barrier(&TEST_PAST_NO_RETURN, "the job never reached its cutover").await;
    assert!(trawl_server::repin::marker_path(&h.data_dir).exists());

    // (No query is issued while the hold runs: the cutover holds every
    // executor permit, and a query into it would wait, not fail. The cancel
    // route reads postgres only.)
    let receipt = match h.schema_admin.schema_repin_cancel().await.expect("cancel") {
        RepinCancel::PastPointOfNoReturn(body) => body,
        other => panic!("expected a refusal, got {other:?}"),
    };
    assert_eq!(receipt.job.as_ref().map(|j| j.id), Some(started.id));
    assert!(
        receipt.detail.contains("point of no return"),
        "{}",
        receipt.detail
    );
    let refused = h.repin_row(started.id).await;
    assert_eq!(
        (refused.cancel_requested_at, refused.cancelled_by),
        (None, None),
        "a refused request leaves no trace on the row"
    );

    hold.release();
    let done = h.wait_terminal(started.id).await;
    assert_eq!(done.status, "succeeded", "error: {:?}", done.error);
    assert_eq!(done.cancel_requested_at, None);
    assert_eq!(done.cancelled_by, None);

    assert_eq!(h.pinned_type("status").await, "VARCHAR");
    assert_eq!(h.count("last=1h | stats count()").await, 2);
    assert_eq!(h.count("status>=400 last=1h | stats count()").await, 1);
    h.wait_cleanup().await;
    assert!(!trawl_server::repin::shadow_root(&h.data_dir).exists());
    assert!(!trawl_server::repin::aside_root(&h.data_dir).exists());
    assert!(!trawl_server::repin::marker_path(&h.data_dir).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crash_between_a_cancel_request_and_its_effect_recovers_as_failed() {
    let h = harness().await;
    h.ingest_and_compact(&[event("api", &json!({"status": 200}))])
        .await;

    let store = h.server.state.storage.repin.clone();
    let claim = trawl_server::store::RepinClaim {
        field: "status",
        from_type: trawl_core::schema::CanonicalType::BigInt,
        to_type: trawl_core::schema::CanonicalType::Varchar,
        dialect: None,
        dry_run: false,
        force: false,
        max_nulled_rows: None,
        max_ambiguous_rows: None,
        requester: Some(&op()),
    };
    let job_id = store.claim(claim).await.expect("claim");
    let requested = store
        .record_cancel_request(job_id, "ops-key")
        .await
        .expect("record the request")
        .expect("the running job takes it");
    assert_eq!(requested.status, RepinJobStatus::Running);

    // The crash state on disk: a building marker over a shadow generation
    // the process never finished.
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let dir = data.join("prod/2026-01-01/10");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("svc.parquet"), b"old generation").unwrap();
    let shadow = trawl_server::repin::shadow_root(&data);
    std::fs::create_dir_all(shadow.join("prod/2026-01-01/10")).unwrap();
    std::fs::write(
        shadow.join("prod/2026-01-01/10/svc.parquet"),
        b"half-written generation",
    )
    .unwrap();
    trawl_server::repin::marker::write_marker(
        &data,
        &trawl_server::repin::RepinMarker {
            job_id,
            field: "status".to_owned(),
            from_type: "BIGINT".to_owned(),
            to_type: "VARCHAR".to_owned(),
            phase: trawl_server::repin::RepinPhase::Building,
        },
    )
    .unwrap();

    let recovered = trawl_server::repin::recover::recover_filesystem(&data, true)
        .unwrap()
        .expect("marker present");
    assert_eq!(
        recovered.action,
        trawl_server::repin::recover::RecoveredAction::AbandonedBuild
    );
    trawl_server::repin::recover::reconcile_store(
        &h.server.state.storage,
        &h.server.state.query.field_catalog,
        &data,
        Some(recovered),
    )
    .await
    .expect("store reconciliation");

    let done = h.repin_row(job_id).await;
    assert_eq!(
        done.status,
        RepinJobStatus::Failed,
        "recovery may not infer `cancelled` from a request nothing acted on"
    );
    assert_eq!(done.cancel_requested_at, requested.cancel_requested_at);
    assert_eq!(done.cancelled_by.as_deref(), Some("ops-key"));
    assert_eq!(
        std::fs::read(data.join("prod/2026-01-01/10/svc.parquet")).unwrap(),
        b"old generation",
        "the live corpus was never touched"
    );
    assert!(!shadow.exists(), "the disposable shadow is swept");
    assert!(!trawl_server::repin::marker_path(&data).exists());

    let next = store.claim(claim).await.expect("the slot is free again");
    assert_ne!(next, job_id);
}

/// One test rather than three: the capture layer is a global subscriber and
/// only the first installer in a process wins.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // three cancel paths, one subscriber
async fn cancel_audit_events_name_the_actor_the_stage_and_the_refusal() {
    use audit_capture::Capture;
    use tracing_subscriber::prelude::*;

    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone().with_filter(
        tracing_subscriber::EnvFilter::new(trawl_server::telemetry::DEFAULT_LOG_FILTER),
    ));
    // Global, not thread-local: the engine runs on other tokio workers and
    // on the blocking pool.
    tracing::subscriber::set_global_default(subscriber).expect("no prior global subscriber");

    let h = hang_guard_harness().await;
    for svc in ["api", "web"] {
        h.ingest_and_compact(&[event(svc, &json!({"status": 200}))])
            .await;
    }

    let hold = Hold::at_progress();
    let cancelled = match h
        .schema_admin
        .schema_repin(
            "status",
            "VARCHAR",
            None,
            false,
            false,
            RepinCeilings::default(),
        )
        .await
        .expect("execute")
    {
        RepinStart::Started(job) => job,
        other => panic!("expected started, got {other:?}"),
    };
    await_barrier(
        &TEST_PROGRESS_PUBLISHED,
        "the build never published progress",
    )
    .await;
    match h.schema_admin.schema_repin_cancel().await.expect("cancel") {
        RepinCancel::Cancelling(_) => {}
        other => panic!("expected an accepted cancel, got {other:?}"),
    }
    h.await_cancel_request(cancelled.id).await;
    hold.release();
    assert_eq!(h.wait_terminal(cancelled.id).await.status, "cancelled");

    // The request event is written by a detached task (a cancel must not be
    // split by a client disconnect), so it is waited for rather than
    // assumed present the instant the 202 lands.
    let requested = capture
        .await_event("repin_cancel_requested")
        .await
        .expect("the accepted request is audited");
    assert!(
        requested.fields["actor"].contains("schema-admin-key"),
        "{requested:?}"
    );
    assert_key_prefix(&requested, &h.server.schema_admin_prefix);
    assert!(requested.fields["job_id"].contains(&cancelled.id.to_string()));

    let effect = capture
        .await_event("repin_cancelled")
        .await
        .expect("the effect site is audited");
    assert!(
        effect.fields["actor"].contains("schema-admin-key"),
        "{effect:?}"
    );
    assert!(
        effect.fields["stage"].contains("build"),
        "the audit names where the cancel landed: {effect:?}"
    );
    assert_key_prefix(&effect, &h.server.schema_admin_prefix);

    h.wait_idle().await;
    let hold = Hold::past_no_return();
    let completing = match h
        .schema_admin
        .schema_repin(
            "status",
            "VARCHAR",
            None,
            false,
            false,
            RepinCeilings::default(),
        )
        .await
        .expect("execute")
    {
        RepinStart::Started(job) => job,
        other => panic!("expected started, got {other:?}"),
    };
    await_barrier(&TEST_PAST_NO_RETURN, "the job never reached its cutover").await;
    match h.schema_admin.schema_repin_cancel().await.expect("cancel") {
        RepinCancel::PastPointOfNoReturn(_) => {}
        other => panic!("expected a refusal, got {other:?}"),
    }
    hold.release();
    assert_eq!(h.wait_terminal(completing.id).await.status, "succeeded");
    let refused = capture
        .await_event("repin_cancel_refused")
        .await
        .expect("the refusal is audited");
    assert!(refused.fields["actor"].contains("schema-admin-key"));
    assert_key_prefix(&refused, &h.server.schema_admin_prefix);
    assert!(refused.fields["job_id"].contains(&completing.id.to_string()));

    let effects_before = capture.count("repin_cancelled");
    let store = h.server.state.storage.repin.clone();
    let job_id = store
        .claim(trawl_server::store::RepinClaim {
            field: "status",
            from_type: trawl_core::schema::CanonicalType::Varchar,
            to_type: trawl_core::schema::CanonicalType::BigInt,
            dialect: None,
            dry_run: false,
            force: true,
            max_nulled_rows: None,
            max_ambiguous_rows: None,
            requester: Some(&op()),
        })
        .await
        .expect("claim");
    store
        .record_cancel_request(job_id, "ops-key")
        .await
        .expect("record the request");
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    std::fs::create_dir_all(data.join("prod/2026-01-01/10")).unwrap();
    std::fs::write(data.join("prod/2026-01-01/10/svc.parquet"), b"old").unwrap();
    trawl_server::repin::marker::write_marker(
        &data,
        &trawl_server::repin::RepinMarker {
            job_id,
            field: "status".to_owned(),
            from_type: "VARCHAR".to_owned(),
            to_type: "BIGINT".to_owned(),
            phase: trawl_server::repin::RepinPhase::Building,
        },
    )
    .unwrap();
    let recovered = trawl_server::repin::recover::recover_filesystem(&data, true)
        .unwrap()
        .expect("marker present");
    trawl_server::repin::recover::reconcile_store(
        &h.server.state.storage,
        &h.server.state.query.field_catalog,
        &data,
        Some(recovered),
    )
    .await
    .expect("store reconciliation");
    assert_eq!(
        h.repin_row(job_id).await.status,
        RepinJobStatus::Failed,
        "the crash state recovers as failed"
    );
    assert_eq!(
        capture.count("repin_cancelled"),
        effects_before,
        "recovery must not audit an effect no boundary observed"
    );
}

async fn start_a_repin_the_finished_shadow_will_refuse(h: &Harness) -> (RepinJobResponse, Hold) {
    // Pin VARCHAR on a text value, then retire the file that carried it:
    // what is left is all numeric text, so the scan projects no loss.
    h.ingest_and_compact(&[event("seed", &json!({"dur": "oops"}))])
        .await;
    assert_eq!(h.pinned_type("dur").await, "VARCHAR");
    let seed = walk(&h.data_dir)
        .into_iter()
        .find(|p| p.file_name().is_some_and(|n| n == "seed.parquet"))
        .expect("seed.parquet exists");
    std::fs::remove_file(&seed).unwrap();
    for svc in ["api", "web"] {
        h.ingest_and_compact(&[event(svc, &json!({"dur": "12"}))])
            .await;
    }

    let hold = Hold::at_progress();
    let started = match h
        .schema_admin
        .schema_repin(
            "dur",
            "BIGINT",
            None,
            false,
            false,
            RepinCeilings::default(),
        )
        .await
        .expect("execute")
    {
        RepinStart::Started(job) => job,
        other => panic!("expected started, got {other:?}"),
    };
    await_barrier(
        &TEST_PROGRESS_PUBLISHED,
        "the build never published progress",
    )
    .await;

    // The late loss, provably after pass 0's snapshot: catch-up folds it
    // in and the finished shadow's gate refuses the cutover.
    h.ingest_and_compact(&[event("api", &json!({"dur": "nope"}))])
        .await;
    (started, hold)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_pending_at_the_force_refusal_takes_the_verdict() {
    let h = hang_guard_harness().await;
    let (started, job_hold) = start_a_repin_the_finished_shadow_will_refuse(&h).await;

    // Hold the job on its decided refusal, which is the only way to be
    // inside the window this asserts on: last file boundary to settlement,
    // microseconds wide when nothing holds it.
    let refusal_hold = Hold::at_force_refusal();
    job_hold.release();
    await_barrier(
        &TEST_FORCE_REFUSAL_REACHED,
        "the finished shadow never refused the cutover",
    )
    .await;

    match h.schema_admin.schema_repin_cancel().await.expect("cancel") {
        RepinCancel::Cancelling(_) => {}
        other => panic!("a job short of its point of no return is cancellable, got {other:?}"),
    }
    refusal_hold.release();

    let done = h.wait_terminal(started.id).await;
    assert_eq!(
        done.status, "cancelled",
        "the accepted cancel outranks the refusal (error: {:?})",
        done.error
    );
    assert_eq!(done.cancelled_by.as_deref(), Some("schema-admin-key"));
    let error = done.error.clone().expect("a cancelled row explains itself");
    assert!(
        error.contains("cancelled by schema-admin-key during build"),
        "{error}"
    );

    assert_eq!(h.pinned_type("dur").await, "VARCHAR");
    assert_eq!(h.count("last=1h | stats count()").await, 3);
    assert_eq!(
        h.count("last=1h | where dur == \"nope\" | stats count()")
            .await,
        1
    );
    assert!(!trawl_server::repin::shadow_root(&h.data_dir).exists());
    assert!(!trawl_server::repin::aside_root(&h.data_dir).exists());
    assert!(!trawl_server::repin::marker_path(&h.data_dir).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_after_the_force_refusal_settled_finds_nothing_to_stop() {
    let h = hang_guard_harness().await;
    let (started, job_hold) = start_a_repin_the_finished_shadow_will_refuse(&h).await;
    job_hold.release();

    // A terminal row is written after settlement, so reading one is the
    // ordering this needs: the refusal has already latched the registry.
    let done = h.wait_terminal(started.id).await;
    assert_eq!(
        done.status, "refused_needs_force",
        "loss that appeared after the scan still needs force (error: {:?})",
        done.error
    );

    match h.schema_admin.schema_repin_cancel().await.expect("cancel") {
        RepinCancel::NoJobRunning(_) => {}
        other => panic!("a settled job has no work left to stop, got {other:?}"),
    }
    let row = h.repin_row(started.id).await;
    assert_eq!(row.status, RepinJobStatus::RefusedNeedsForce);
    assert_eq!(
        (row.cancel_requested_at, row.cancelled_by),
        (None, None),
        "a request that took no effect leaves no trace on the row"
    );
    assert_eq!(h.pinned_type("dur").await, "VARCHAR");
}

fn assert_key_prefix(event: &audit_capture::Captured, prefix: &str) {
    let seen = event
        .fields
        .get("actor_key_prefix")
        .unwrap_or_else(|| panic!("no actor_key_prefix on {event:?}"));
    assert!(
        seen.contains(prefix),
        "expected the acting key's prefix {prefix:?} in {event:?}"
    );
}

mod audit_capture {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Debug, Clone)]
    pub struct Captured {
        pub fields: BTreeMap<String, String>,
    }

    #[derive(Clone, Default)]
    pub struct Capture {
        events: Arc<Mutex<Vec<Captured>>>,
    }

    impl Capture {
        fn matching(&self, event_type: &str) -> Vec<Captured> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter(|e| {
                    e.fields
                        .get("event_type")
                        .is_some_and(|t| t.contains(event_type))
                })
                .cloned()
                .collect()
        }

        pub fn count(&self, event_type: &str) -> usize {
            self.matching(event_type).len()
        }

        pub async fn await_event(&self, event_type: &str) -> Option<Captured> {
            let start = std::time::Instant::now();
            while start.elapsed() < super::HANG_GUARD {
                if let Some(found) = self.matching(event_type).into_iter().next() {
                    return Some(found);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            None
        }
    }

    struct Visitor<'a>(&'a mut BTreeMap<String, String>);

    impl tracing::field::Visit for Visitor<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name().to_owned(), format!("{value:?}"));
        }
    }

    impl<S> tracing_subscriber::Layer<S> for Capture
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = BTreeMap::new();
            event.record(&mut Visitor(&mut fields));
            self.events.lock().unwrap().push(Captured { fields });
        }
    }
}

impl Harness {
    fn engine(&self) -> std::sync::Arc<RepinEngine> {
        self.server
            .state
            .repin
            .clone()
            .expect("an ingest-enabled node owns a repin engine")
    }

    async fn job_row(&self, id: i64) -> trawl_server::store::RepinJob {
        self.server
            .state
            .storage
            .repin
            .get(id)
            .await
            .expect("job read")
            .expect("job row")
    }
}

async fn varchar_corpus(h: &Harness, numeric: &[&str], lossy: &[&str]) {
    let mut events = Vec::new();
    for v in numeric {
        events.push(event("api", &json!({ "dur": v })));
    }
    for v in lossy {
        events.push(event("api", &json!({ "dur": v })));
    }
    h.ingest_and_compact(&events).await;
    assert_eq!(h.pinned_type("dur").await, "VARCHAR");
}

#[tokio::test(flavor = "multi_thread")]
async fn stated_ceilings_travel_over_the_wire_and_the_accepted_pair_returns() {
    let h = harness().await;
    varchar_corpus(&h, &["12"], &["oops"]).await;

    let report = match h
        .schema_admin
        .schema_repin(
            "dur",
            "BIGINT",
            None,
            true,
            true,
            RepinCeilings {
                max_nulled_rows: Some(0),
                max_ambiguous_rows: None,
            },
        )
        .await
        .expect("dry run")
    {
        RepinStart::Report(job) => job,
        other => panic!("expected a report, got {other:?}"),
    };
    assert_eq!(report.max_nulled_rows, Some(0), "the request echoes back");
    assert_eq!(report.max_ambiguous_rows, None, "unstated stays absent");
    assert_eq!(report.accepted_max_nulled_rows, Some(0), "explicit wins");
    assert!(
        report.accepted_max_ambiguous_rows.is_some(),
        "an unstated ceiling still resolves: {report:?}"
    );
    assert_eq!(report.requires_force, Some(true));
    let reason = report.requires_force_reason.expect("a reason");
    assert!(reason.contains("accepted 0"), "{reason}");

    let err = h
        .schema_admin
        .schema_repin(
            "dur",
            "BIGINT",
            None,
            true,
            false,
            RepinCeilings {
                max_nulled_rows: Some(5),
                max_ambiguous_rows: None,
            },
        )
        .await
        .expect_err("a ceiling without force is a bad request");
    assert!(format!("{err}").contains("400"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forced_repin_proceeds_at_exactly_the_ceiling_it_accepted() {
    let h = harness().await;
    varchar_corpus(&h, &["12"], &["oops"]).await;

    let engine = h.engine();
    let started = match engine
        .start(
            "dur",
            "BIGINT",
            None,
            false,
            true,
            RequestedCeilings {
                max_nulled: Some(1),
                max_ambiguous: None,
            },
            Some(&op()),
        )
        .await
        .expect("forced start")
    {
        StartOutcome::Started(job) => job,
        other => panic!("expected a started job, got {other:?}"),
    };
    let done = h.wait_terminal(started.id).await;
    assert_eq!(done.status, "succeeded", "error: {:?}", done.error);
    assert_eq!(done.rows_nulled, 1, "the loss is exactly what was accepted");
    assert_eq!(h.pinned_type("dur").await, "BIGINT");

    let row = h.job_row(started.id).await;
    assert_eq!(row.max_nulled_rows, Some(1), "the request's own number");
    assert_eq!(
        row.accepted_max_nulled_rows,
        Some(1),
        "an explicit ceiling is what the job is held to"
    );
    assert_eq!(
        row.accepted_max_ambiguous_rows,
        Some(10),
        "the unstated dimension keeps its scan-derived default"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_ceiling_below_the_corpus_refuses_naming_accepted_and_actual() {
    let h = harness().await;
    varchar_corpus(&h, &["12"], &["oops", "nah"]).await;

    let engine = h.engine();
    let refused = match engine
        .start(
            "dur",
            "BIGINT",
            None,
            false,
            true,
            RequestedCeilings {
                max_nulled: Some(1),
                max_ambiguous: None,
            },
            Some(&op()),
        )
        .await
        .expect("a refusal is an outcome, not an error")
    {
        StartOutcome::Refused(job) => job,
        other => panic!("expected a refusal, got {other:?}"),
    };
    assert_eq!(
        refused.status,
        trawl_server::store::RepinJobStatus::RefusedNeedsForce
    );
    let reason = refused.error.expect("the refusal carries its reason");
    assert!(reason.contains("accepted 1"), "{reason}");
    assert!(reason.contains("2 nulled row(s)"), "{reason}");

    assert_eq!(h.pinned_type("dur").await, "VARCHAR", "corpus untouched");
    assert_eq!(
        h.count("last=1h | where dur == \"nah\" | stats count()")
            .await,
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_explicit_zero_ceiling_refuses_before_anything_is_staged() {
    let h = harness().await;
    varchar_corpus(&h, &["12"], &["oops"]).await;

    let engine = h.engine();
    let refused = match engine
        .start(
            "dur",
            "BIGINT",
            None,
            false,
            true,
            RequestedCeilings {
                max_nulled: Some(0),
                max_ambiguous: Some(0),
            },
            Some(&op()),
        )
        .await
        .expect("a refusal is an outcome, not an error")
    {
        StartOutcome::Refused(job) => job,
        other => panic!("expected a refusal, got {other:?}"),
    };
    let reason = refused.error.expect("the refusal carries its reason");
    assert!(reason.contains("accepted 0"), "{reason}");

    assert!(!trawl_server::repin::shadow_root(&h.data_dir).exists());
    assert!(!trawl_server::repin::aside_root(&h.data_dir).exists());
    assert!(!trawl_server::repin::marker_path(&h.data_dir).exists());
    assert_eq!(h.pinned_type("dur").await, "VARCHAR");

    let row = h.job_row(refused.id).await;
    assert_eq!(row.accepted_max_nulled_rows, Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn boolean_force_resolves_the_ceiling_from_the_scan() {
    let h = harness().await;
    varchar_corpus(&h, &["12"], &["oops"]).await;

    let engine = h.engine();
    let dry = match engine
        .start(
            "dur",
            "BIGINT",
            None,
            true,
            true,
            RequestedCeilings::default(),
            Some(&op()),
        )
        .await
        .expect("forced dry run")
    {
        StartOutcome::DryRun(job) => job,
        other => panic!("expected a dry-run report, got {other:?}"),
    };
    assert_eq!(dry.projected_nulls, 1, "`oops` has no BIGINT reading");
    let row = h.job_row(dry.id).await;
    assert_eq!(row.max_nulled_rows, None, "the request stated nothing");
    assert_eq!(
        row.accepted_max_nulled_rows,
        Some(11),
        "one projected null plus the floor of ten"
    );
    assert_eq!(
        row.accepted_max_ambiguous_rows,
        Some(10),
        "a zero-count dimension is still the floor, never unlimited"
    );

    let started = match engine
        .start(
            "dur",
            "BIGINT",
            None,
            false,
            true,
            RequestedCeilings::default(),
            Some(&op()),
        )
        .await
        .expect("forced execute")
    {
        StartOutcome::Started(job) => job,
        other => panic!("expected a started job, got {other:?}"),
    };
    let done = h.wait_terminal(started.id).await;
    assert_eq!(done.status, "succeeded", "error: {:?}", done.error);
    assert_eq!(done.rows_nulled, 1);
    assert_eq!(h.pinned_type("dur").await, "BIGINT");
}

#[tokio::test(flavor = "multi_thread")]
async fn mid_build_growth_past_the_default_ceiling_refuses_the_cutover() {
    let h = harness().await;

    // Pin VARCHAR on a text value, then retire that file: what is left is an
    // all-numeric corpus, so the scan projects no loss and the ceiling is
    // the bare floor of ten.
    h.ingest_and_compact(&[event("seed", &json!({"dur": "oops"}))])
        .await;
    assert_eq!(h.pinned_type("dur").await, "VARCHAR");
    let seed_file = walk(&h.data_dir)
        .into_iter()
        .find(|p| p.file_name().is_some_and(|n| n == "seed.parquet"))
        .expect("seed.parquet exists");
    std::fs::remove_file(&seed_file).unwrap();

    // Enough affected files that the build is still running when the late
    // batch lands.
    for svc in ["api", "web", "worker", "edge", "db", "cache"] {
        h.ingest_and_compact(&[event(svc, &json!({"dur": "12"}))])
            .await;
    }

    let engine = h.engine();
    trawl_server::repin::engine::TEST_FILE_DELAY_MS
        .store(400, std::sync::atomic::Ordering::Relaxed);
    let started = match engine
        .start(
            "dur",
            "BIGINT",
            None,
            false,
            true,
            RequestedCeilings::default(),
            Some(&op()),
        )
        .await
        .expect("forced execute")
    {
        StartOutcome::Started(job) => job,
        other => panic!("expected a started job, got {other:?}"),
    };
    assert_eq!(
        h.job_row(started.id).await.accepted_max_nulled_rows,
        Some(10),
        "a lossless scan accepts the floor and nothing more"
    );

    // Eleven values the new pin cannot read, ingested and compacted during
    // the build: one row past what force accepted.
    let late: Vec<_> = (0..11)
        .map(|i| event("late", &json!({ "dur": format!("nope{i}") })))
        .collect();
    h.ingest_and_compact(&late).await;

    let done = h.wait_terminal(started.id).await;
    trawl_server::repin::engine::TEST_FILE_DELAY_MS.store(0, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        done.status, "refused_needs_force",
        "force covers the plan it was shown, not whatever arrives later \
         (error: {:?})",
        done.error
    );
    let reason = done.error.expect("the refusal carries its reason");
    assert!(reason.contains("accepted 10"), "{reason}");
    assert!(reason.contains("11 nulled row(s)"), "{reason}");
    assert!(
        reason.contains("force it with ceilings"),
        "an operator who already passed force is told to raise the number, \
         not to pass force: {reason}"
    );
    assert_eq!(
        done.rows_nulled, 11,
        "the terminal write carries the tallies the gate refused on, so the \
         verdict and its evidence cannot disagree on the wire"
    );

    assert_eq!(h.pinned_type("dur").await, "VARCHAR");
    assert_eq!(h.count("last=1h | stats count()").await, 17);
    assert!(!trawl_server::repin::shadow_root(&h.data_dir).exists());
    assert!(!trawl_server::repin::aside_root(&h.data_dir).exists());
    assert!(!trawl_server::repin::marker_path(&h.data_dir).exists());
}

#[tokio::test(flavor = "multi_thread")]
// Three jobs in one body: the capture layer is a global subscriber, so the
// forced, refused and unforced cases have to share a process to prove the
// record fires for exactly one of them.
#[allow(clippy::too_many_lines)]
async fn the_repin_audit_records_forced_cutovers_and_recovered_ack_clears() {
    use common::audit_capture::Capture;
    use tracing_subscriber::prelude::*;

    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(
        capture
            .clone()
            .with_filter(tracing_subscriber::EnvFilter::new("trawl_server=info")),
    );
    // Global, not thread-local: the job runs detached on other workers.
    tracing::subscriber::set_global_default(subscriber).expect("no prior global subscriber");

    let h = harness().await;
    varchar_corpus(&h, &["12"], &["oops"]).await;
    let engine = h.engine();

    let refused = match engine
        .start(
            "dur",
            "BIGINT",
            None,
            false,
            true,
            RequestedCeilings {
                max_nulled: Some(0),
                max_ambiguous: None,
            },
            Some(&op()),
        )
        .await
        .expect("refusal")
    {
        StartOutcome::Refused(job) => job,
        other => panic!("expected a refusal, got {other:?}"),
    };

    let forced = match engine
        .start(
            "dur",
            "BIGINT",
            None,
            false,
            true,
            RequestedCeilings::default(),
            Some(&op()),
        )
        .await
        .expect("forced execute")
    {
        StartOutcome::Started(job) => job,
        other => panic!("expected a started job, got {other:?}"),
    };
    let done = h.wait_terminal(forced.id).await;
    assert_eq!(done.status, "succeeded", "error: {:?}", done.error);

    h.ingest_and_compact(&[event("api", &json!({"code": 200}))])
        .await;
    assert_eq!(h.pinned_type("code").await, "BIGINT");
    h.wait_idle().await;
    let unforced = match engine
        .start(
            "code",
            "VARCHAR",
            None,
            false,
            false,
            RequestedCeilings::default(),
            Some(&op()),
        )
        .await
        .expect("unforced execute")
    {
        StartOutcome::Started(job) => job,
        other => panic!("expected a started job, got {other:?}"),
    };
    let done = h.wait_terminal(unforced.id).await;
    assert_eq!(done.status, "succeeded", "error: {:?}", done.error);

    let accepted = capture.of_type("repin_force_accepted", "field", "dur");
    assert_eq!(
        accepted.len(),
        1,
        "one forced cutover, one record (refused and unforced jobs leave \
         none): {accepted:?}"
    );
    let record = &accepted[0];
    let field = |name: &str| {
        record
            .fields
            .get(name)
            .unwrap_or_else(|| panic!("{name} missing from {record:?}"))
            .clone()
    };
    assert_eq!(field("job_id"), forced.id.to_string());
    assert!(field("field").contains("dur"));
    assert!(field("from").contains("VARCHAR"));
    assert!(field("to").contains("BIGINT"));
    assert_eq!(field("accepted_max_nulled_rows"), "11");
    assert_eq!(field("accepted_max_ambiguous_rows"), "10");
    assert_eq!(field("rows_nulled"), "1", "what the rewrite actually did");
    assert_eq!(field("ambiguous_numerals"), "0");
    assert_eq!(field("scanned_projected_nulls"), "1");
    assert_eq!(field("scanned_ambiguous_numerals"), "0");
    assert!(field("requested_by").contains("op"));
    assert!(
        !record.fields.contains_key("unmapped_samples"),
        "the record is counts only, never sample values: {record:?}"
    );
    assert!(refused.error.is_some(), "the refused job kept its reason");

    // The other place a cutover completes: boot recovery. The replay finishes
    // the flip, including the ack clear, which owes the same audit record
    // the live path emits.
    let catalog = &h.server.state.storage.catalog;
    for _ in 0..3 {
        catalog
            .record_conflicts(&[trawl_server::store::FieldConflict {
                field: "code".to_owned(),
                service: "api".to_owned(),
                observed_type: "VARCHAR".to_owned(),
                expected_type: trawl_core::schema::CanonicalType::BigInt,
                rows_nulled: 3,
                samples: Vec::new(),
            }])
            .await
            .unwrap();
    }
    let mut conn =
        <sqlx::postgres::PgConnection as sqlx::Connection>::connect(&h.server.app_db_url)
            .await
            .expect("connect app db");
    sqlx::query(
        "UPDATE field_conflict_stats SET first_at = now() - interval '48 hours'
         WHERE field = 'code'",
    )
    .execute(&mut conn)
    .await
    .expect("backdate the evidence");
    catalog
        .acknowledge_degraded_field("code", "key-aaa", None)
        .await
        .unwrap();

    let recovered_job = h
        .server
        .state
        .storage
        .repin
        .claim(trawl_server::store::RepinClaim {
            field: "code",
            from_type: trawl_core::schema::CanonicalType::Varchar,
            to_type: trawl_core::schema::CanonicalType::BigInt,
            dialect: None,
            dry_run: false,
            force: false,
            requester: None,
            max_nulled_rows: None,
            max_ambiguous_rows: None,
        })
        .await
        .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    std::fs::create_dir_all(data.join("prod")).unwrap();
    let replay_marker = trawl_server::repin::RepinMarker {
        job_id: recovered_job,
        field: "code".to_owned(),
        from_type: "VARCHAR".to_owned(),
        to_type: "BIGINT".to_owned(),
        phase: trawl_server::repin::RepinPhase::Cutover,
    };
    let replay = |marker: trawl_server::repin::RepinMarker| {
        let data = data.clone();
        let storage = h.server.state.storage.clone();
        let cache = h.server.state.query.field_catalog.clone();
        async move {
            trawl_server::repin::marker::write_marker(&data, &marker).unwrap();
            let recovered = trawl_server::repin::recover::recover_filesystem(&data, true)
                .unwrap()
                .expect("marker present");
            trawl_server::repin::recover::reconcile_store(&storage, &cache, &data, Some(recovered))
                .await
                .expect("store reconciliation");
        }
    };
    replay(replay_marker.clone()).await;

    let cleared = capture.of_type("field_degraded_ack_cleared", "field", "code");
    assert_eq!(
        cleared.len(),
        1,
        "the recovered cutover audits the ack it cleared: {cleared:?}"
    );
    assert!(cleared[0].field("reason").contains("repin"));
    assert_eq!(cleared[0].field("job_id"), recovered_job.to_string());
    assert!(catalog.degraded_ack("code").await.unwrap().is_none());

    replay(replay_marker).await;
    assert_eq!(
        capture
            .of_type("field_degraded_ack_cleared", "field", "code")
            .len(),
        1,
        "a boot replay must not re-announce a clear that happened once"
    );
}

use trawl_server::repin::JobReading;

impl Harness {
    fn repin_jobs(&self) -> JobReading {
        self.server.state.repin_jobs.read()
    }

    /// Polled from the job's release, not from its terminal row: a success row
    /// lands before the post-cutover sweep, the gauge reset and the outcome
    /// count, and the generation guard drops after all three.
    async fn await_jobs_idle(&self) -> JobReading {
        let start = std::time::Instant::now();
        while start.elapsed() < HANG_GUARD {
            let reading = self.repin_jobs();
            if !reading.in_flight() {
                assert!(!trawl_server::repin::marker_path(&self.data_dir).exists());
                assert!(!trawl_server::repin::shadow_root(&self.data_dir).exists());
                assert!(!trawl_server::repin::aside_root(&self.data_dir).exists());
                return reading;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the repin job generation never read idle again within {HANG_GUARD:?}");
    }

    async fn start_a_held_repin(&self) -> (RepinJobResponse, Hold) {
        for svc in ["api", "web", "worker"] {
            self.ingest_and_compact(&[
                event(svc, &json!({"status": 200})),
                event(svc, &json!({"status": 404})),
            ])
            .await;
        }
        let hold = Hold::at_progress();
        let started = match self
            .schema_admin
            .schema_repin(
                "status",
                "VARCHAR",
                None,
                false,
                false,
                RepinCeilings::default(),
            )
            .await
            .expect("execute")
        {
            RepinStart::Started(job) => job,
            other => panic!("expected started, got {other:?}"),
        };
        await_barrier(
            &TEST_PROGRESS_PUBLISHED,
            "the build never published progress",
        )
        .await;
        (started, hold)
    }
}

fn assert_one_job_running(running: JobReading, idle: JobReading) {
    assert!(running.in_flight());
    assert_eq!(running.running(), 1);
    assert_eq!(running.changes_since(idle), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_repin_job_generation_brackets_a_completed_job() {
    let h = hang_guard_harness().await;
    h.ingest_and_compact(&[event("api", &json!({"status": 200}))])
        .await;
    let idle = h.repin_jobs();
    assert!(!idle.in_flight());

    let report = dry_run(&h.schema_admin, "status", "VARCHAR", None, false).await;
    assert_eq!(report.status, "succeeded");
    assert_eq!(h.repin_jobs().changes_since(idle), 0);

    let (started, hold) = h.start_a_held_repin().await;
    assert_one_job_running(h.repin_jobs(), idle);
    hold.release();
    assert_eq!(h.await_jobs_idle().await.changes_since(idle), 2);
    let done = h.wait_terminal(started.id).await;
    assert_eq!(done.status, "succeeded", "error: {:?}", done.error);
    assert_eq!(h.pinned_type("status").await, "VARCHAR");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_repin_job_generation_brackets_a_cancelled_job() {
    let h = hang_guard_harness().await;
    let idle = h.repin_jobs();
    let (started, hold) = h.start_a_held_repin().await;
    assert_one_job_running(h.repin_jobs(), idle);
    match h.schema_admin.schema_repin_cancel().await.expect("cancel") {
        RepinCancel::Cancelling(_) => {}
        other => panic!("expected an accepted cancel, got {other:?}"),
    }
    h.await_cancel_request(started.id).await;
    hold.release();
    assert_eq!(h.await_jobs_idle().await.changes_since(idle), 2);
    let done = h.wait_terminal(started.id).await;
    assert_eq!(done.status, "cancelled", "error: {:?}", done.error);
    assert_eq!(h.pinned_type("status").await, "BIGINT");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_repin_job_generation_brackets_a_refused_job() {
    let h = hang_guard_harness().await;
    let idle = h.repin_jobs();
    let (started, hold) = start_a_repin_the_finished_shadow_will_refuse(&h).await;
    assert_one_job_running(h.repin_jobs(), idle);
    hold.release();
    assert_eq!(h.await_jobs_idle().await.changes_since(idle), 2);
    let done = h.wait_terminal(started.id).await;
    assert_eq!(
        done.status, "refused_needs_force",
        "error: {:?}",
        done.error
    );
    assert_eq!(h.pinned_type("dur").await, "VARCHAR");
}

use trawl_server::repin::{CancelActor, CancelVerdict, JobHold, JobHolds};

const PERMIT_HELD: &str =
    "the previous repin is still running or still cleaning up its staging; retry when it finishes";

#[derive(Debug, Clone, Copy)]
enum HoldPoint {
    BeforePostSwapSweep,
    BeforePermitRelease,
}

/// Dropping it disarms the hold, which also frees a job it holds now. Bind
/// it as a local of the test body, so unwinding drops it before the runtime
/// does.
struct EngineHold {
    engine: std::sync::Arc<RepinEngine>,
    point: HoldPoint,
}

impl EngineHold {
    fn arm(engine: &std::sync::Arc<RepinEngine>, point: HoldPoint) -> Self {
        let owner = Self {
            engine: std::sync::Arc::clone(engine),
            point,
        };
        owner.hold().arm();
        owner
    }

    fn hold(&self) -> &JobHold {
        let holds: &JobHolds = self.engine.holds();
        match self.point {
            HoldPoint::BeforePostSwapSweep => &holds.before_post_swap_sweep,
            HoldPoint::BeforePermitRelease => &holds.before_permit_release,
        }
    }

    async fn reached(&self) -> i64 {
        let what = format!("a repin job reaches {:?}", self.point);
        trawl_server::test_timing::until(&what, || self.hold().held_job().is_some()).await;
        self.hold().held_job().expect("a job latched the hold")
    }

    fn release(&self) {
        self.hold().release();
    }
}

impl Drop for EngineHold {
    fn drop(&mut self) {
        self.hold().disarm();
    }
}

#[derive(Debug, PartialEq)]
struct RepinOwnership {
    rows: i64,
    newest: String,
    marker: Option<Vec<u8>>,
    shadow: Option<Vec<String>>,
    aside: Option<Vec<String>>,
    rollup_paused: bool,
    cancel_owner: CancelVerdict,
}

fn staging_listing(root: &std::path::Path) -> Option<Vec<String>> {
    if !root.exists() {
        return None;
    }
    let mut files: Vec<String> = walk(root)
        .into_iter()
        .map(|path| {
            path.strip_prefix(root)
                .expect("a staging path under its root")
                .display()
                .to_string()
        })
        .collect();
    files.sort();
    Some(files)
}

impl Harness {
    async fn repin_ownership(&self) -> RepinOwnership {
        use sqlx::Connection as _;

        let mut observer = sqlx::postgres::PgConnection::connect(&self.server.app_db_url)
            .await
            .expect("connect app db");
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM repin_jobs")
            .fetch_one(&mut observer)
            .await
            .expect("count jobs");
        let newest = self
            .engine()
            .store()
            .latest()
            .await
            .expect("read the newest job");
        RepinOwnership {
            rows,
            newest: format!("{newest:?}"),
            marker: std::fs::read(trawl_server::repin::marker_path(&self.data_dir)).ok(),
            shadow: staging_listing(&trawl_server::repin::shadow_root(&self.data_dir)),
            aside: staging_listing(&trawl_server::repin::aside_root(&self.data_dir)),
            rollup_paused: self
                .server
                .state
                .ingest
                .repin_coordinator
                .as_ref()
                .expect("ingest-enabled server has a repin coordinator")
                .rollup_paused(),
            // A cancel request names the armed job without changing it:
            // past the point of no return it is refused, and with nothing
            // armed there is nothing to change.
            cancel_owner: self
                .engine()
                .cancel(&CancelActor::new("operator", "test-key")),
        }
    }

    async fn repin_to_bigint(
        &self,
        dry_run: bool,
    ) -> (
        Result<RepinStart, trawl_client::ClientError>,
        std::time::Duration,
    ) {
        let asked = std::time::Instant::now();
        let answer = self
            .schema_admin
            .schema_repin(
                "status",
                "BIGINT",
                None,
                dry_run,
                false,
                RepinCeilings::default(),
            )
            .await;
        (answer, asked.elapsed())
    }

    async fn assert_route_refuses(&self, owner: i64, dry_run: bool) {
        let (answer, took) = self.repin_to_bigint(dry_run).await;
        assert_permit_refusal(owner, dry_run, &answer, took);
    }

    async fn assert_engine_refuses(&self, owner: i64, dry_run: bool) {
        use trawl_server::error::ServerError;

        match self
            .engine()
            .start(
                "status",
                "BIGINT",
                None,
                dry_run,
                false,
                RequestedCeilings::default(),
                Some(&op()),
            )
            .await
        {
            Err(ServerError::Conflict(message)) => assert_eq!(message, PERMIT_HELD),
            other => panic!(
                "job {owner} still holds the repin permit, yet RepinEngine::start \
                 (dry_run={dry_run}) answered {other:?}"
            ),
        }
    }
}

fn assert_permit_refusal(
    owner: i64,
    dry_run: bool,
    answer: &Result<RepinStart, trawl_client::ClientError>,
    took: std::time::Duration,
) {
    match answer {
        Err(trawl_client::ClientError::Server { status, error, .. }) => {
            assert_eq!(
                (*status, error.message.as_str()),
                (409, PERMIT_HELD),
                "dry_run={dry_run}"
            );
        }
        other => panic!(
            "job {owner} still holds the repin permit, yet the route answered a \
             repin request (dry_run={dry_run}) with {other:?}"
        ),
    }
    assert!(
        took < Duration::from_secs(DEFAULT_TEST_TIMEOUT_SECS),
        "the refusal took {took:?}, past the route's bound"
    );
}

/// Read per file, so a mixed-type corpus cannot hide behind a union.
fn stored_status(file: &std::path::Path) -> (String, Vec<i64>) {
    let conn = duckdb::Connection::open_in_memory().expect("duckdb");
    let path = file.to_string_lossy().replace('\'', "''");
    let stored: String = conn
        .query_row(
            &format!(
                "SELECT column_type FROM (DESCRIBE SELECT status FROM read_parquet('{path}'))"
            ),
            [],
            |row| row.get(0),
        )
        .expect("describe status");
    let mut values = conn
        .prepare(&format!(
            "SELECT status FROM read_parquet('{path}') ORDER BY status"
        ))
        .expect("prepare status read");
    let values = values
        .query_map([], |row| row.get::<_, i64>(0))
        .expect("read status")
        .collect::<Result<Vec<_>, _>>()
        .expect("status values are integers");
    (stored, values)
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one held job, eight refusals, and the job that follows
async fn cleanup_excludes_next_repin_until_owner_release() {
    let h = harness().await;
    for svc in ["api", "web", "worker"] {
        h.ingest_and_compact(&[
            event(svc, &json!({"status": 200})),
            event(svc, &json!({"status": 404})),
        ])
        .await;
    }
    assert_eq!(h.pinned_type("status").await, "BIGINT");
    let engine = h.engine();
    let coordinator = h
        .server
        .state
        .ingest
        .repin_coordinator
        .as_ref()
        .expect("ingest-enabled server has a repin coordinator");

    // Armed before A starts: each latches the first job that reaches it.
    let sweep = EngineHold::arm(&engine, HoldPoint::BeforePostSwapSweep);
    let permit = EngineHold::arm(&engine, HoldPoint::BeforePermitRelease);
    let a = match h
        .schema_admin
        .schema_repin(
            "status",
            "VARCHAR",
            None,
            false,
            false,
            RepinCeilings::default(),
        )
        .await
        .expect("start job A")
    {
        RepinStart::Started(job) => job,
        other => panic!("expected started, got {other:?}"),
    };
    assert_eq!(
        sweep.reached().await,
        a.id,
        "job A is held before its sweep"
    );

    assert_eq!(h.repin_row(a.id).await.status, RepinJobStatus::Succeeded);
    assert_eq!(h.pinned_type("status").await, "VARCHAR");
    let marker = trawl_server::repin::marker::read_marker(&h.data_dir)
        .expect("read the marker")
        .expect("job A's marker stands");
    assert_eq!(
        (marker.job_id, marker.phase),
        (a.id, trawl_server::repin::RepinPhase::Cleanup)
    );
    assert!(trawl_server::repin::aside_root(&h.data_dir).exists());
    let owned = h.repin_ownership().await;
    assert!(owned.rollup_paused, "job A still pauses the rollup");
    assert_eq!(
        owned.cancel_owner,
        CancelVerdict::PastPointOfNoReturn { job_id: a.id }
    );

    // A build without the permit admits this request. When it does, hold
    // the admitted job at its first published progress, with its marker
    // and shadow on disk, let A's sweep run, and fail on what is left.
    let admitted_build = Hold::at_progress();
    let (first, took) = h.repin_to_bigint(false).await;
    if let Ok(RepinStart::Started(b)) = &first {
        await_barrier(
            &TEST_PROGRESS_PUBLISHED,
            "the admitted job never published progress",
        )
        .await;
        let marker_before = trawl_server::repin::marker::read_marker(&h.data_dir);
        let shadow_before = staging_listing(&trawl_server::repin::shadow_root(&h.data_dir));
        sweep.release();
        assert_eq!(permit.reached().await, a.id, "job A ends its task");
        panic!(
            "job {} was admitted while job {} still owned its staging. Job A's \
             sweep then ran. The marker before it: {marker_before:?}; after it: \
             {:?}. The shadow root before it: {shadow_before:?}; after it: {:?}",
            b.id,
            a.id,
            trawl_server::repin::marker::read_marker(&h.data_dir),
            staging_listing(&trawl_server::repin::shadow_root(&h.data_dir)),
        );
    }
    assert_permit_refusal(a.id, false, &first, took);
    drop(admitted_build);
    h.assert_route_refuses(a.id, true).await;
    h.assert_engine_refuses(a.id, false).await;
    h.assert_engine_refuses(a.id, true).await;
    assert_eq!(
        h.repin_ownership().await,
        owned,
        "a refused repin changed what job A owns"
    );

    sweep.release();
    assert_eq!(permit.reached().await, a.id, "job A ends its task");
    assert!(!trawl_server::repin::marker_path(&h.data_dir).exists());
    assert!(!trawl_server::repin::shadow_root(&h.data_dir).exists());
    assert!(!trawl_server::repin::aside_root(&h.data_dir).exists());
    assert!(!coordinator.rollup_paused());
    assert!(!h.repin_jobs().in_flight());
    assert_eq!(
        engine.cancel(&CancelActor::new("operator", "test-key")),
        CancelVerdict::NoJobRunning
    );
    assert!(!engine.repin_idle(), "job A's task still holds the permit");
    let rows = h.repin_ownership().await.rows;
    h.assert_route_refuses(a.id, false).await;
    h.assert_engine_refuses(a.id, true).await;
    assert_eq!(h.repin_ownership().await.rows, rows, "no refusal claims");

    permit.release();
    h.wait_idle().await;
    let b = match h.repin_to_bigint(false).await.0.expect("start job B") {
        RepinStart::Started(job) => job,
        other => panic!("expected started, got {other:?}"),
    };
    let done = h.wait_terminal(b.id).await;
    assert_eq!(done.status, "succeeded", "error: {:?}", done.error);
    h.wait_idle().await;
    assert!(!trawl_server::repin::marker_path(&h.data_dir).exists());
    assert!(!trawl_server::repin::shadow_root(&h.data_dir).exists());
    assert!(!trawl_server::repin::aside_root(&h.data_dir).exists());
    assert_eq!(h.pinned_type("status").await, "BIGINT");
    assert_eq!(
        h.count("last=1h | where status == 200 | stats count()")
            .await,
        3
    );
    assert_eq!(h.count("status>=400 last=1h | stats count()").await, 3);
    let files: Vec<_> = walk(&h.data_dir)
        .into_iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "parquet"))
        .collect();
    assert_eq!(files.len(), 3, "one file per service: {files:?}");
    for file in files {
        let label = file.display().to_string();
        let stored = tokio::task::spawn_blocking(move || stored_status(&file))
            .await
            .expect("read a corpus file");
        assert_eq!(stored, ("BIGINT".to_owned(), vec![200, 404]), "{label}");
    }
}

const SHADOW_LEFT: &str =
    "a repin shadow root remains with no marker; restart trawld and boot removes it";

const ASIDE_LEFT: &str = "a repin aside root remains with no marker, and trawld never deletes \
                          one without a marker; remove the data root's .repin-aside sibling by \
                          hand, then retry";

const STAGING_UNREADABLE: &str = "the repin staging state could not be read, so no repin can \
                                  start; the server log names the error";

const UNCLAIMED_AT_BOUND: &str = "the repin request could not claim a job within [server] \
                                  timeout_secs, so no repin runs from it; retry the repin";

fn marker_left(job_id: i64, phase: &str) -> String {
    format!(
        "repin job {job_id} did not finish cleaning up (its marker stands in phase {phase}); \
         fix the cause the server log reports, then restart trawld so boot replays the \
         cleanup; trawld --doctor reports the marker under server.recovery.repin"
    )
}

impl Harness {
    async fn engine_repin(
        &self,
        to: &str,
        dry_run: bool,
    ) -> Result<StartOutcome, trawl_server::error::ServerError> {
        self.engine()
            .start(
                "status",
                to,
                None,
                dry_run,
                false,
                RequestedCeilings::default(),
                Some(&op()),
            )
            .await
    }

    async fn assert_route_answers(&self, to: &str, dry_run: bool, status: u16, message: &str) {
        let asked = std::time::Instant::now();
        let answer = self
            .schema_admin
            .schema_repin("status", to, None, dry_run, false, RepinCeilings::default())
            .await;
        let took = asked.elapsed();
        match &answer {
            Err(trawl_client::ClientError::Server {
                status: got, error, ..
            }) => {
                assert_eq!(
                    (*got, error.message.as_str()),
                    (status, message),
                    "dry_run={dry_run}: {error:?}"
                );
                self.assert_names_no_path(&format!("{error:?}"));
            }
            other => panic!(
                "expected {status} {message:?} for a repin to {to} (dry_run={dry_run}), \
                 got {other:?}"
            ),
        }
        assert!(
            took < Duration::from_secs(DEFAULT_TEST_TIMEOUT_SECS),
            "the refusal took {took:?}, past the route's bound"
        );
    }

    async fn assert_engine_conflict(&self, to: &str, dry_run: bool, message: &str) {
        match self.engine_repin(to, dry_run).await {
            Err(trawl_server::error::ServerError::Conflict(got)) => {
                assert_eq!(got, message, "dry_run={dry_run}");
                self.assert_names_no_path(&got);
            }
            other => panic!(
                "expected a Conflict {message:?} for a repin to {to} (dry_run={dry_run}), \
                 got {other:?}"
            ),
        }
    }

    fn assert_names_no_path(&self, text: &str) {
        let root = self.data_dir.parent().expect("the data root has a parent");
        for path in [&self.data_dir, &root.to_path_buf()] {
            let path = path.display().to_string();
            assert!(!text.contains(&path), "{text:?} names the path {path}");
        }
    }

    async fn assert_refusal_wrote_nothing(&self, before: &RepinOwnership) {
        let after = self.repin_ownership().await;
        assert_eq!(&after, before, "a refused repin changed the repin state");
        assert!(!after.rollup_paused, "a refused repin took a rollup pause");
        assert_eq!(
            after.cancel_owner,
            CancelVerdict::NoJobRunning,
            "a refused repin armed the cancel registry"
        );
        assert!(
            self.engine().repin_idle(),
            "a refused repin kept the permit"
        );
    }

    async fn seed_status_corpus(&self) {
        for svc in ["api", "web", "worker"] {
            self.ingest_and_compact(&[
                event(svc, &json!({"status": 200})),
                event(svc, &json!({"status": 404})),
            ])
            .await;
        }
        assert_eq!(self.pinned_type("status").await, "BIGINT");
    }

    async fn assert_status_corpus_is_bigint(&self) {
        assert_eq!(self.pinned_type("status").await, "BIGINT");
        assert_eq!(
            self.count("last=1h | where status == 200 | stats count()")
                .await,
            3
        );
        assert_eq!(self.count("status>=400 last=1h | stats count()").await, 3);
        let files: Vec<_> = walk(&self.data_dir)
            .into_iter()
            .filter(|path| path.extension().is_some_and(|ext| ext == "parquet"))
            .collect();
        assert_eq!(files.len(), 3, "one file per service: {files:?}");
        for file in files {
            let label = file.display().to_string();
            let stored = tokio::task::spawn_blocking(move || stored_status(&file))
                .await
                .expect("read a corpus file");
            assert_eq!(stored, ("BIGINT".to_owned(), vec![200, 404]), "{label}");
        }
    }

    /// A claim released from a lock lands a moment later, so the newest row
    /// until then is an older job's.
    async fn wait_latest_terminal(&self, after: Option<i64>) -> RepinJob {
        let store = &self.server.state.storage.repin;
        let start = std::time::Instant::now();
        while start.elapsed() < HANG_GUARD {
            if let Some(job) = store.latest().await.expect("read the newest job")
                && after.is_none_or(|older| job.id > older)
                && job.status != RepinJobStatus::Running
            {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the newest repin job never left running within {HANG_GUARD:?}");
    }
}

/// The aside root is swapped for a regular file, so the sweep's
/// `remove_dir_all` fails with ENOTDIR (as it does for root too, unlike a
/// mode-bit fault).
///
/// The restart is boot's own recovery pair run by hand: `Restartable` boots
/// through `boot::prepare_corpus`, which does not run it. Keeping the engine
/// also keeps its permit, so the repin that follows proves the stranded
/// job's permit was released, not replaced by a fresh engine's.
#[tokio::test(flavor = "multi_thread")]
async fn stranded_cleanup_blocks_until_recovery() {
    let h = harness().await;
    h.seed_status_corpus().await;
    let engine = h.engine();

    let sweep = EngineHold::arm(&engine, HoldPoint::BeforePostSwapSweep);
    let a = match h
        .schema_admin
        .schema_repin(
            "status",
            "VARCHAR",
            None,
            false,
            false,
            RepinCeilings::default(),
        )
        .await
        .expect("start job A")
    {
        RepinStart::Started(job) => job,
        other => panic!("expected started, got {other:?}"),
    };
    assert_eq!(
        sweep.reached().await,
        a.id,
        "job A is held before its sweep"
    );
    let aside = trawl_server::repin::aside_root(&h.data_dir);
    std::fs::remove_dir_all(&aside).expect("remove job A's aside");
    std::fs::write(&aside, b"not a directory").expect("plant a file where the aside was");
    sweep.release();
    h.wait_idle().await;

    let marker = trawl_server::repin::marker::read_marker(&h.data_dir)
        .expect("read the marker")
        .expect("job A's marker stands over its failed sweep");
    assert_eq!(
        (marker.job_id, marker.phase),
        (a.id, trawl_server::repin::RepinPhase::Cleanup)
    );
    assert!(aside.is_file(), "the planted file survived the sweep");
    assert_eq!(h.repin_row(a.id).await.status, RepinJobStatus::Succeeded);
    assert_eq!(h.pinned_type("status").await, "VARCHAR");

    let stranded = marker_left(a.id, "cleanup");
    let before = h.repin_ownership().await;
    h.assert_route_answers("BIGINT", false, 409, &stranded)
        .await;
    h.assert_route_answers("BIGINT", true, 409, &stranded).await;
    h.assert_engine_conflict("BIGINT", false, &stranded).await;
    h.assert_engine_conflict("BIGINT", true, &stranded).await;
    h.assert_refusal_wrote_nothing(&before).await;

    std::fs::remove_file(&aside).expect("remove the planted file");
    h.assert_engine_conflict("BIGINT", true, &stranded).await;

    let recovered = trawl_server::repin::recover::recover_filesystem(&h.data_dir, true)
        .expect("boot's filesystem recovery")
        .expect("the stranded marker is recovered");
    trawl_server::repin::recover::reconcile_store(
        &h.server.state.storage,
        &h.server.state.query.field_catalog,
        &h.data_dir,
        Some(recovered),
    )
    .await
    .expect("boot's store reconciliation");
    assert!(!trawl_server::repin::marker_path(&h.data_dir).exists());
    assert!(!trawl_server::repin::shadow_root(&h.data_dir).exists());
    assert!(!aside.exists());

    let b = match h
        .schema_admin
        .schema_repin(
            "status",
            "BIGINT",
            None,
            false,
            false,
            RepinCeilings::default(),
        )
        .await
        .expect("a repin after the replay is admitted")
    {
        RepinStart::Started(job) => job,
        other => panic!("expected started, got {other:?}"),
    };
    let done = h.wait_terminal(b.id).await;
    assert_eq!(done.status, "succeeded", "error: {:?}", done.error);
    h.wait_idle().await;
    assert!(!trawl_server::repin::marker_path(&h.data_dir).exists());
    h.assert_status_corpus_is_bigint().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn leftover_staging_without_marker_refuses() {
    let h = harness().await;
    h.seed_status_corpus().await;
    let before_any = h.repin_ownership().await;
    assert_eq!(before_any.rows, 0);

    for (root, message) in [
        (trawl_server::repin::shadow_root(&h.data_dir), SHADOW_LEFT),
        (trawl_server::repin::aside_root(&h.data_dir), ASIDE_LEFT),
    ] {
        std::fs::create_dir(&root).expect("plant a marker-less staging root");
        std::fs::write(root.join("left.parquet"), b"an earlier job's").unwrap();
        let before = h.repin_ownership().await;
        assert_eq!(before.marker, None);

        h.assert_route_answers("VARCHAR", false, 409, message).await;
        h.assert_route_answers("VARCHAR", true, 409, message).await;
        h.assert_engine_conflict("VARCHAR", false, message).await;
        h.assert_engine_conflict("VARCHAR", true, message).await;
        h.assert_refusal_wrote_nothing(&before).await;
        assert_eq!(h.repin_ownership().await.rows, 0, "no refusal claims");

        std::fs::remove_dir_all(&root).expect("remove the planted root");
    }
    assert_eq!(h.repin_ownership().await, before_any);

    match h.engine_repin("VARCHAR", true).await {
        Ok(StartOutcome::DryRun(job)) => assert_eq!(job.status, RepinJobStatus::Succeeded),
        other => panic!("with the staging gone a dry run is admitted: {other:?}"),
    }
    assert_eq!(h.pinned_type("status").await, "BIGINT");
}

/// `data/REPIN` is a symlink to itself, so `try_exists` fails with ELOOP,
/// as it does for root too.
#[tokio::test(flavor = "multi_thread")]
async fn unreadable_staging_evidence_answers_503() {
    use trawl_server::error::ServerError;

    let h = harness().await;
    h.seed_status_corpus().await;
    let marker = trawl_server::repin::marker_path(&h.data_dir);
    std::os::unix::fs::symlink("REPIN", &marker).expect("plant a self-referencing marker");
    assert!(
        marker.try_exists().is_err(),
        "the planted marker reads as ELOOP"
    );
    let before = h.repin_ownership().await;
    assert_eq!(before.rows, 0);

    for dry_run in [false, true] {
        match h.engine_repin("VARCHAR", dry_run).await {
            Err(ServerError::ServiceUnavailable(message)) => {
                assert_eq!(message, STAGING_UNREADABLE, "dry_run={dry_run}");
                h.assert_names_no_path(&message);
            }
            other => panic!("unreadable staging answers 503 (dry_run={dry_run}): {other:?}"),
        }
        h.assert_route_answers("VARCHAR", dry_run, 503, STAGING_UNREADABLE)
            .await;
    }
    h.assert_refusal_wrote_nothing(&before).await;
    assert_eq!(
        std::fs::read_link(&marker).expect("the planted link stands"),
        std::path::PathBuf::from("REPIN"),
        "no refusal wrote a marker over it"
    );

    std::fs::remove_file(&marker).expect("remove the planted link");
    match h.engine_repin("VARCHAR", true).await {
        Ok(StartOutcome::DryRun(job)) => assert_eq!(job.status, RepinJobStatus::Succeeded),
        other => panic!("with the link gone a dry run is admitted: {other:?}"),
    }
}

/// `SHARE` conflicts with the claim's INSERT and with no read.
async fn lock_out_claims(
    conn: &mut sqlx::postgres::PgConnection,
) -> sqlx::Transaction<'_, sqlx::Postgres> {
    use sqlx::Connection as _;

    let mut lock = conn.begin().await.expect("begin");
    sqlx::query("LOCK TABLE repin_jobs IN SHARE MODE")
        .execute(&mut *lock)
        .await
        .expect("hold the claim");
    lock
}

impl Harness {
    async fn wait_claim_waiting(&self) {
        use sqlx::Connection as _;

        let mut observer = sqlx::postgres::PgConnection::connect(&self.server.app_db_url)
            .await
            .expect("connect app db");
        let start = std::time::Instant::now();
        while start.elapsed() < HANG_GUARD {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(DISTINCT pid) FROM pg_locks
                 WHERE NOT granted
                   AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
            )
            .fetch_one(&mut observer)
            .await
            .expect("count waiting claims");
            if waiting > 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no claim waited on the lock within {HANG_GUARD:?}");
    }

    async fn assert_permit_still_held(&self, to: &str, what: &str) {
        assert!(!self.engine().repin_idle(), "{what}: the permit reads free");
        self.assert_engine_conflict(to, true, PERMIT_HELD).await;
        self.assert_route_answers(to, false, 409, PERMIT_HELD).await;
        assert!(!self.engine().repin_idle(), "{what}: the permit reads free");
    }
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // three owners, each held, probed and released
async fn admission_owner_survives_disconnect_and_blocking_work() {
    use sqlx::Connection as _;
    use trawl_server::error::ServerError;

    let h = harness().await;
    h.seed_status_corpus().await;
    let engine = h.engine();
    let requester = op();
    let mut blocker = sqlx::postgres::PgConnection::connect(&h.server.app_db_url)
        .await
        .expect("connect app db");

    // 1. The client disconnects mid-claim.
    let lock = lock_out_claims(&mut blocker).await;
    let mut request = Box::pin(engine.start_within(
        Duration::from_secs(60),
        "status",
        "VARCHAR",
        None,
        true,
        false,
        RequestedCeilings::default(),
        Some(&requester),
    ));
    if let Some(early) = until_polling("the request takes the permit", &mut request, || {
        !engine.repin_idle()
    })
    .await
    {
        panic!("the claim is held, so the request cannot answer yet: {early:?}");
    }
    drop(request);
    h.wait_claim_waiting().await;
    h.assert_permit_still_held("VARCHAR", "the client went away mid-claim")
        .await;
    lock.rollback().await.expect("release the claim");
    let late = h.wait_latest_terminal(None).await;
    assert_eq!(late.status, RepinJobStatus::Blocked, "{late:?}");
    assert!(late.planned_at.is_none(), "it never scanned: {late:?}");
    let disconnected = late.id;
    h.wait_idle().await;

    // 2. The request's bound passes mid-claim.
    let lock = lock_out_claims(&mut blocker).await;
    match engine
        .start_within(
            Duration::from_millis(200),
            "status",
            "VARCHAR",
            None,
            true,
            false,
            RequestedCeilings::default(),
            Some(&requester),
        )
        .await
    {
        Err(ServerError::ServiceUnavailable(message)) => {
            assert_eq!(message, UNCLAIMED_AT_BOUND);
        }
        other => panic!("a claim pending at the bound answers 503: {other:?}"),
    }
    h.wait_claim_waiting().await;
    h.assert_permit_still_held("VARCHAR", "the request's bound passed mid-claim")
        .await;
    lock.rollback().await.expect("release the claim");
    let late = h.wait_latest_terminal(Some(disconnected)).await;
    assert_eq!(late.status, RepinJobStatus::Blocked, "{late:?}");
    assert!(late.planned_at.is_none(), "it never scanned: {late:?}");
    h.wait_idle().await;

    // 3. A blocking staging step is still running.
    let sweep = EngineHold::arm(&engine, HoldPoint::BeforePostSwapSweep);
    let a = match h
        .schema_admin
        .schema_repin(
            "status",
            "VARCHAR",
            None,
            false,
            false,
            RepinCeilings::default(),
        )
        .await
        .expect("start job A")
    {
        RepinStart::Started(job) => job,
        other => panic!("expected started, got {other:?}"),
    };
    assert_eq!(sweep.reached().await, a.id, "job A is held in its sweep");
    h.assert_permit_still_held("BIGINT", "job A's sweep is still running")
        .await;
    assert_eq!(
        sweep.hold().held_job(),
        Some(a.id),
        "job A is still inside its sweep's closure"
    );
    assert!(trawl_server::repin::marker_path(&h.data_dir).exists());
    sweep.release();
    h.wait_idle().await;
    assert!(!trawl_server::repin::marker_path(&h.data_dir).exists());
    assert!(!trawl_server::repin::aside_root(&h.data_dir).exists());
    let done = h.wait_terminal(a.id).await;
    assert_eq!(done.status, "succeeded", "error: {:?}", done.error);
    assert_eq!(h.repin_ownership().await.rows, 3, "two blocked, one run");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_panicked_decision_task_releases_the_permit() {
    use trawl_server::error::ServerError;

    let h = harness().await;
    h.seed_status_corpus().await;
    let engine = h.engine();
    let coordinator = h
        .server
        .state
        .ingest
        .repin_coordinator
        .as_ref()
        .expect("ingest-enabled server has a repin coordinator");

    engine.holds().panic_next_decision();
    match h.engine_repin("VARCHAR", false).await {
        Err(err @ ServerError::Panicked("repin job")) => assert_eq!(err.error_class(), "panic"),
        other => panic!("a panicked decision task answers as a panic: {other:?}"),
    }
    let job = engine
        .store()
        .latest()
        .await
        .expect("read the newest job")
        .expect("the claim left a row");
    assert_eq!(job.status, RepinJobStatus::Failed, "{job:?}");
    assert_eq!(job.error.as_deref(), Some("repin job task panicked"));
    assert!(!coordinator.rollup_paused(), "the unwind dropped the pause");
    assert_eq!(
        engine.cancel(&CancelActor::new("operator", "test-key")),
        CancelVerdict::NoJobRunning
    );
    assert!(!trawl_server::repin::marker_path(&h.data_dir).exists());
    assert!(!trawl_server::repin::shadow_root(&h.data_dir).exists());

    h.wait_idle().await;
    match h.engine_repin("VARCHAR", true).await {
        Ok(StartOutcome::DryRun(report)) => {
            assert_eq!(report.status, RepinJobStatus::Succeeded);
        }
        other => panic!("a repin after the panic is admitted: {other:?}"),
    }
    assert_eq!(h.pinned_type("status").await, "BIGINT");
}
