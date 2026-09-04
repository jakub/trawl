// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end repin tests: a real TLS server over a per-test data root,
//! ingest through HTTP, compaction driven per-tick with the catalog and the
//! repin coordinator wired exactly as trawld wires them.

mod common;

use std::os::unix::fs::MetadataExt as _;
use std::time::Duration;

use common::{TestServer, setup_in_dir_with_data};
use serde_json::json;
use trawl_client::{HttpClient, RepinStart};
use trawl_server::catalog::CatalogContext;
use trawl_server::config::RateLimitConfig;

fn now_ts() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

/// One `/metrics` scrape off the running server (unauthenticated route).
async fn scrape_metrics(url: &str) -> String {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
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

/// Read an unlabelled gauge's value out of a scrape body.
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
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let wal_dir = root.join("wal");
    let data_dir = root.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let data_glob = format!("{}/**/*.parquet", data_dir.display());

    let server = setup_in_dir_with_data(&root, data_glob, RateLimitConfig::default()).await;
    std::mem::forget(tmp); // outlives the server; OS cleans up

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
    /// One compaction tick with the catalog and the repin coordinator
    /// wired, exactly as trawld's loop runs it.
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

    /// Count from a `| stats count()` query.
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

    /// Poll the status surface until the job with `id` leaves `running`.
    async fn wait_terminal(&self, id: i64) -> trawl_client::RepinJobResponse {
        for _ in 0..600 {
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
        panic!("repin job {id} did not reach a terminal state in time");
    }

    /// Poll until the `data/REPIN` marker is gone. On the SUCCESS path the
    /// row terminalizes inside `finish_cutover`'s transaction while the
    /// detached task is still sweeping staging, so `wait_terminal` is not a
    /// barrier for filesystem cleanup; the marker is removed last (it is
    /// what licenses deleting the staging roots), so its absence is.
    async fn wait_cleanup(&self) {
        for _ in 0..600 {
            if !trawl_server::repin::marker_path(&self.data_dir).exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("repin marker still present after cleanup budget");
    }

    /// The type `/api/v1/schema` advertises for a field, read off the
    /// TTL-cached column listing. [`Harness::pinned_type`] reads
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

    /// The catalog's advertised type for a field, via `/api/v1/schema/fields`
    /// (no TTL cache in front of it).
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

/// The acceptance test: a BIGINT-pinned field with a shelved conflict
/// value is repinned to VARCHAR. The dry run projects, the rewrite
/// matches the projection, existing queries answer identically, the
/// shelved value becomes queryable, unaffected files keep their inodes,
/// and a foreign file rides across untouched.
#[tokio::test(flavor = "multi_thread")]
async fn repin_is_invisible_to_queries_and_resurrects_shelved_values() {
    let h = harness().await;

    // First typed sight pins BIGINT; the enum-shaped batch then conflicts
    // and its value is shelved into _raw.
    h.ingest_and_compact(&[
        event("api", &json!({"status": 200})),
        event("api", &json!({"status": 404})),
    ])
    .await;
    h.ingest_and_compact(&[event("api", &json!({"status": "accepted"}))])
        .await;
    // An unaffected service (no `status` field at all).
    h.ingest_and_compact(&[event("quiet", &json!({"other": 1}))])
        .await;
    assert_eq!(h.pinned_type("status").await, "BIGINT");

    // A foreign parquet inside the env dir but off the layout: it must
    // ride the swap byte-identical (hardlinked, same inode), never be
    // rewritten.
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

    // Record answers under the BIGINT pin.
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

    // Dry run: one affected file, one resurrectable value, nothing lost
    // (VARCHAR is the always-lossless target).
    let dry = match h
        .schema_admin
        .schema_repin("status", "varchar", None, true, false)
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

    // Execute; the dry-run projection is the rewrite's outcome.
    let started = match h
        .schema_admin
        .schema_repin("status", "VARCHAR", None, false, false)
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

    // The pin flipped.
    assert_eq!(h.pinned_type("status").await, "VARCHAR");

    // The invisibility promise: identical answers.
    assert_eq!(h.count(q_ordered).await, before_ordered);
    assert_eq!(h.count(q_eq).await, before_eq);
    // The payoff: the shelved value is back in the structured column —
    // present, and queryable by the equality that had no answer for it
    // under the numeric pin.
    assert_eq!(h.count(q_present).await, 3);
    assert_eq!(h.count(q_accepted).await, 1);

    // Unaffected file: same inode (hardlinked, never rewritten).
    assert_eq!(
        std::fs::metadata(&quiet_file).unwrap().ino(),
        quiet_ino,
        "unaffected files ride as hardlinks"
    );
    // Foreign file: byte-identical, same inode.
    assert_eq!(
        std::fs::read(&foreign).unwrap(),
        b"not even parquet, and not trawl's"
    );
    assert_eq!(std::fs::metadata(&foreign).unwrap().ino(), foreign_ino);

    // Staging is swept and the marker is gone.
    assert!(!trawl_server::repin::shadow_root(&h.data_dir).exists());
    assert!(!trawl_server::repin::aside_root(&h.data_dir).exists());
    assert!(!trawl_server::repin::marker_path(&h.data_dir).exists());

    // And ingest still works against the new pin: a numeric wire value
    // conforms to VARCHAR text.
    h.ingest_and_compact(&[event("api", &json!({"status": 503}))])
        .await;
    assert_eq!(h.count("status>=400 last=1h | stats count()").await, 2);
}

/// `/api/v1/schema` caches its unscoped column listing for
/// `schema_cache_ttl_secs`, and the cached entry carries the pin generation
/// it was built under. A cutover therefore invalidates it at once, instead
/// of leaving the endpoint advertising the old type for up to a TTL while
/// the corpus it describes has already been rewritten.
///
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
        .schema_repin("status", "VARCHAR", None, false, false)
        .await
        .expect("execute")
    {
        RepinStart::Started(job) => job,
        other => panic!("expected started, got {other:?}"),
    };
    let done = h.wait_terminal(started.id).await;
    assert_eq!(done.status, "succeeded", "error: {:?}", done.error);

    // No sleep: the primed entry is still well inside its TTL, and it must
    // not be served.
    assert_eq!(
        h.schema_endpoint_type("status").await,
        "VARCHAR",
        "/schema must retype the moment the cutover flips the pin"
    );
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

/// The force gate, both halves: a lossy repin refuses with the plan
/// attached (HTTP 409, job terminal `refused_needs_force`, corpus
/// untouched), and proceeds under `--force` with exact null accounting
/// plus conflict evidence.
#[tokio::test(flavor = "multi_thread")]
async fn lossy_repin_refuses_without_force_and_accounts_with_it() {
    let h = harness().await;

    // A half-numeric text field pins VARCHAR (the ladder needs >=90%).
    h.ingest_and_compact(&[
        event("api", &json!({"dur": "12"})),
        event("api", &json!({"dur": "oops"})),
    ])
    .await;
    assert_eq!(h.pinned_type("dur").await, "VARCHAR");

    // Lossy without force: 409, plan attached, nothing changed.
    let refused = match h
        .schema_admin
        .schema_repin("dur", "BIGINT", None, false, false)
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

    // With force: exact accounting, and the loss lands as conflict
    // evidence like any lossy conform.
    let started = match h
        .schema_admin
        .schema_repin("dur", "BIGINT", None, false, true)
        .await
        .expect("forced execute")
    {
        RepinStart::Started(job) => job,
        other => panic!("expected started, got {other:?}"),
    };
    let done = h.wait_terminal(started.id).await;
    assert_eq!(done.status, "succeeded", "error: {:?}", done.error);
    assert_eq!(done.rows_nulled, 1);
    assert_eq!(h.pinned_type("dur").await, "BIGINT");

    let conflicts = h
        .query
        .catalog_conflicts(Some("dur"), None, None, None)
        .await
        .expect("conflicts");
    assert!(
        conflicts
            .conflicts
            .iter()
            .any(|c| c.expected_type == "BIGINT" && c.rows_nulled == 1),
        "a forced lossy repin records its losses: {:?}",
        conflicts.conflicts
    );

    // The numeric survivor still answers.
    assert_eq!(
        h.count("last=1h | where dur == 12 | stats count()").await,
        1
    );
    // The lost value stays findable in _raw (whole-event search).
    assert_eq!(h.count("oops last=1h | stats count()").await, 1);
}

/// The force gate is asked of the finished shadow, not just the pre-build
/// scan: a lossless plan whose corpus grows a non-conforming value while
/// the rewrite runs is refused at the cutover, corpus untouched — and the
/// same repin proceeds under force.
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
        .schema_repin("dur", "BIGINT", None, true, false)
        .await
        .expect("dry run")
    {
        RepinStart::Report(job) => job,
        other => panic!("expected a dry-run report, got {other:?}"),
    };
    assert_eq!(dry.projected_nulls, 0, "the scanned corpus is all numeric");

    // Execute without force on that lossless plan.
    trawl_server::repin::engine::TEST_FILE_DELAY_MS
        .store(400, std::sync::atomic::Ordering::Relaxed);
    let started = match h
        .schema_admin
        .schema_repin("dur", "BIGINT", None, false, false)
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

    // Corpus untouched: the old pin, every row, and no staging left.
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

    // The operator's answer: the same repin, forced, accepts the loss.
    let forced = match h
        .schema_admin
        .schema_repin("dur", "BIGINT", None, false, true)
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

/// A caller that walks away mid-scan must not strand the one-running
/// slot. The scan is a full-corpus `DuckDB` pass, so a client or proxy
/// timeout drops the request future long before it finishes: the claimed
/// job's whole ladder therefore runs detached, terminalizes on its own,
/// and leaves the next repin acceptable instead of 409ing every request
/// until a daemon restart reconciles the orphan.
#[tokio::test(flavor = "multi_thread")]
async fn a_disconnected_caller_does_not_strand_the_running_slot() {
    use std::sync::atomic::Ordering;
    use trawl_server::repin::engine::TEST_SCAN_DELAY_MS;

    let h = harness().await;
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
    TEST_SCAN_DELAY_MS.store(500, Ordering::Relaxed);
    let mut start = Box::pin(engine.start("status", "VARCHAR", None, true, false, Some("op")));
    // Let the claim land and the scan begin, then drop the future exactly
    // as hyper drops a handler whose connection went away.
    assert!(
        tokio::time::timeout(Duration::from_millis(600), &mut start)
            .await
            .is_err(),
        "the scan must still be running when the caller disconnects"
    );
    drop(start);
    TEST_SCAN_DELAY_MS.store(0, Ordering::Relaxed);

    // The abandoned job still reaches a terminal state on its own. Polled
    // through the store, not the status route: an orphaned slot would
    // otherwise spend the poller's rate-limit bucket before the deadline
    // and report a 429 instead of the stranded job.
    let store = h.server.state.storage.repin.clone();
    let id = store
        .latest()
        .await
        .expect("latest job")
        .expect("the claim left a job row")
        .id;
    let mut terminal = None;
    for _ in 0..300 {
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

    // And the slot is free: the next repin is served, not 409ed.
    match h
        .schema_admin
        .schema_repin("status", "VARCHAR", None, true, false)
        .await
        .expect("the running slot is free again")
    {
        RepinStart::Report(_) => {}
        other => panic!("expected a dry-run report, got {other:?}"),
    }
}

/// One repin at a time: a concurrent second request 409s with the error
/// envelope (not a refusal plan); ingest and queries ride through the
/// rewrite — events land in the hot buffer immediately and exactly once in
/// the post-cutover corpus, and a query loop across the rest of the job
/// (release, catch-up, cutover, sweep) never errors. `/metrics` is scraped
/// from inside the rewrite: the running gauge is up and the
/// `files_total`/`files_done` progress pair is readable while the job is
/// still running, with the outcome counter landing only at the terminal
/// state.
///
/// Synchronised by ORDERING, not by timing (issue #79 review). The mid-job
/// state this test observes — job row `running`, running gauge up,
/// `files_done` ≥ 1 — exists only between the end of the first build pass
/// (progress is published per PASS) and the job's terminal write, and a
/// polling observer can miss that window or find the job already finished
/// on its first read; both were reproducible here by removing the per-file
/// delay, and `retries = 1` is why CI saw it as a flake rather than a
/// failure. The build now HOLDS at its first published progress until this
/// test releases it, so every mid-job assertion below is a fact about
/// order. No sleeps, and no per-file delay at all.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // the sqlx macro used to hide the body in an inner fn
async fn ingest_queries_and_a_second_repin_ride_through_a_slow_rewrite() {
    use std::sync::atomic::Ordering;

    let h = harness().await;

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
    // until this test releases it, the job cannot terminalize, so every
    // "during the rewrite" step below is during the rewrite by
    // construction.
    trawl_server::repin::engine::TEST_PROGRESS_PUBLISHED.store(false, Ordering::SeqCst);
    trawl_server::repin::engine::TEST_RELEASE_JOB.store(false, Ordering::SeqCst);
    trawl_server::repin::engine::TEST_HOLD_AFTER_PROGRESS.store(true, Ordering::SeqCst);

    let started = match h
        .schema_admin
        .schema_repin("status", "VARCHAR", None, false, false)
        .await
        .expect("execute")
    {
        RepinStart::Started(job) => job,
        other => panic!("expected started, got {other:?}"),
    };

    // A second repin while one runs: the one-running index answers 409
    // through the error envelope.
    let second = h
        .schema_admin
        .schema_repin("status", "DOUBLE", None, false, false)
        .await;
    match second {
        Err(trawl_client::ClientError::Server { status, .. }) => assert_eq!(status, 409),
        other => panic!("expected a 409 error envelope, got {other:?}"),
    }

    // Ingest during the build: visible immediately via the hot buffer.
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
    // And fold it into the corpus mid-build: the catch-up loop must pick
    // the new/replaced file up (the compact tick runs under the corpus
    // gate, so it can never straddle the cutover).
    h.compact_tick().await;

    // The one state a mid-job scrape needs, pinned: wait for the hold.
    for _ in 0..600 {
        if trawl_server::repin::engine::TEST_PROGRESS_PUBLISHED.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        trawl_server::repin::engine::TEST_PROGRESS_PUBLISHED.load(Ordering::SeqCst),
        "the build never published progress"
    );
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
    // Queries still answer inside the held build — nothing is excluded yet.
    assert_eq!(h.count("last=1h | stats count()").await, 7);
    let mid_job = scrape_metrics(&h.server.url).await;

    // Progress is scrapeable mid-job: the running gauge is up and real
    // per-file progress is already on it, by construction rather than by
    // catching a window.
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

    // Query across the rest of the job — the release, the catch-up passes,
    // the cutover's exclusion window and the sweep. No query may error.
    let query_loop = {
        let client = HttpClient::new_insecure(&h.server.url, &h.server.analyst_token).unwrap();
        let engine_store = h.server.state.storage.repin.clone();
        let id = started.id;
        tokio::spawn(async move {
            let mut queries = 0u32;
            loop {
                let result = client
                    .query_paginated("last=1h | stats count()", None, None)
                    .await;
                assert!(result.is_ok(), "query errored mid-repin: {result:?}");
                queries += 1;
                let job = engine_store.get(id).await.expect("job row").expect("job");
                if job.status != trawl_server::store::RepinJobStatus::Running {
                    return queries;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
    };

    trawl_server::repin::engine::TEST_RELEASE_JOB.store(true, Ordering::SeqCst);
    let done = h.wait_terminal(started.id).await;
    assert_eq!(done.status, "succeeded", "error: {:?}", done.error);
    let queries = query_loop.await.expect("query loop");
    assert!(queries > 0, "the loop queried across the running job");

    // Exactly once: everything ingested before and during the job.
    assert_eq!(h.pinned_type("status").await, "VARCHAR");
    assert_eq!(h.count("last=1h | stats count()").await, 7);
    assert_eq!(h.count("status=418 last=1h | stats count()").await, 1);

    // ...and the outcome landed once the job terminalized.
    let metrics = scrape_metrics(&h.server.url).await;
    assert_eq!(
        gauge_value(&metrics, trawl_server::metrics::CATALOG_REPIN_RUNNING),
        Some(0.0),
        "the running gauge must fall back to 0 at the terminal state"
    );
    assert!(
        metrics.contains("trawl_catalog_repin_jobs_total{outcome=\"succeeded\"}"),
        "outcome counter missing"
    );
    assert!(metrics.contains("trawl_catalog_repin_duration_seconds"));
}

/// The final pause defers WAL draining, and does nothing else an operator
/// can observe. Inside the held pause (corpus gate + every executor permit)
/// ingest still lands, the hot buffer still holds those events undrained
/// (a drain follows a compaction batch, and no batch may run), and a query
/// issued into the pause waits rather than erroring or missing them. A
/// compaction batch that starts inside the pause is blocked, not starved
/// and not dropped: it resumes on its own the moment the pause lifts, with
/// no new tick and no operator action, and drains exactly the events it
/// held. Past the cutover those events are in the corpus exactly once,
/// before and after that drain lands: ADR-0008's invisible-events
/// prohibition across the one stopped world a cutover needs.
///
/// (Draining is deferred rather than continuous because the cutover takes
/// the corpus gate's write side, which excludes whole compaction batches —
/// the mechanism correction recorded in ADR-0011's 2026-08-12 amendment:
/// a mixed-type corpus silently promotes instead of erring, so exclusion
/// is the entire atomicity budget.)
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // the sqlx macro used to hide the body in an inner fn
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

    // Widen the pause so the test can act inside it.
    coordinator.set_cutover_hold_ms(3_000);
    let started = match h
        .schema_admin
        .schema_repin("status", "VARCHAR", None, false, false)
        .await
        .expect("execute")
    {
        RepinStart::Started(job) => job,
        other => panic!("expected started, got {other:?}"),
    };

    // Wait for the rising edge into the pause.
    let mut entered = false;
    for _ in 0..600 {
        if coordinator.cutover_hold_active() {
            entered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(entered, "the cutover never entered its pause");

    // Ingest into the pause: the WAL writer and the hot buffer take
    // neither the corpus gate nor an executor permit, so this must not
    // block on the cutover.
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

    // A query issued into the pause waits for a permit; it must answer,
    // and it must see that event. Spawned, because the pause is exactly
    // what it is waiting on.
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

    // Nothing drains while the pause is held: a drain only follows a
    // compaction batch, and no batch may start under the corpus gate.
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

    // (That answer is the pre-drain half of exactly-once: the event was
    // still only in the hot buffer, and the paused query saw it once.)
    assert_eq!(h.pinned_type("status").await, "VARCHAR");

    // The deferred batch resumes itself: no second tick was issued, no
    // operator acted, and the drain it was holding completes.
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

    // Exactly once in the repinned corpus, now that the batch it deferred
    // has published and drained. (Counted here rather than mid-publish: a
    // batch in flight may briefly show an event in both its new parquet
    // and the not-yet-drained hot snapshot — ADR-0008's accepted transient
    // duplicate, ordinary compaction, nothing repin does.)
    assert_eq!(h.count("last=1h | stats count()").await, 3);
    assert_eq!(h.count("status=418 last=1h | stats count()").await, 1);
}

/// The postgres half of boot recovery over a real storage state: a marker
/// in the cutover phase finishes the job transactionally, flips the pin,
/// re-arms the conformance pass, sweeps the aside, and removes the
/// marker; orphaned running rows without a marker fail.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // the sqlx macro used to hide the body in an inner fn
async fn boot_reconciliation_completes_a_recovered_cutover() {
    let h = harness().await;

    // A pinned custom field to flip.
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
            requested_by: None,
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

    // Boot replay, both halves.
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

    // The swap completed forward, the pin flipped everywhere, staging and
    // marker are gone, and the conformance pass is re-armed.
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
    // is the only thing licensing trawl to delete it, and a stranded root
    // suppresses retention and makes the next cutover's forward-only swap
    // ambiguous.
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

    // An orphaned running row (no marker) fails at the same boot step.
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
            requested_by: None,
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

/// A resurrection-only pass (`to == current`, force): the shelved value
/// comes back without changing the pin — the supported repair for a
/// boot-conformed interrupted repin.
#[tokio::test(flavor = "multi_thread")]
async fn resurrection_only_pass_recovers_without_retyping() {
    let h = harness().await;
    h.ingest_and_compact(&[event("api", &json!({"status": 200}))])
        .await;
    h.ingest_and_compact(&[event("api", &json!({"status": "accepted"}))])
        .await;
    assert_eq!(h.pinned_type("status").await, "BIGINT");

    // Same type without force is a 400 (nothing to do without intent).
    let err = h
        .schema_admin
        .schema_repin("status", "BIGINT", None, false, false)
        .await
        .expect_err("same-type without force refuses");
    match err {
        trawl_client::ClientError::Server { status, .. } => assert_eq!(status, 400),
        other => panic!("expected 400, got {other:?}"),
    }

    // With force: a resurrection-only rewrite. `"accepted"` still has no
    // BIGINT reading — it stays shelved and counts as nulled-projection
    // zero, since it was already null — but a recoverable value would
    // return.
    let dry = match h
        .schema_admin
        .schema_repin("status", "BIGINT", None, true, true)
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

/// Boot recovery over a SEVERITY cutover marker (issue #79): the engine
/// writes the marker with the CATALOG spelling, so recovery has to read it
/// back that way. The physical parse has no `SEVERITY` spelling at all,
/// which made this the one replay path that could REFUSE — and it refuses
/// after the corpus is already half-swapped, where forward is the only safe
/// direction.
#[tokio::test(flavor = "multi_thread")]
async fn boot_reconciliation_replays_a_severity_cutover() {
    let h = harness().await;

    // A sender field an operator repins onto the ladder.
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
            requested_by: None,
        })
        .await
        .unwrap();

    // A throwaway root crashed mid-cutover: the shadow holds the new
    // generation, the marker names the `SEVERITY` target as the engine
    // spells it (`CanonicalType::as_catalog`).
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

    // Forward: the swap finished, the pin flipped in postgres and in the
    // in-process cache, and the job completed.
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

/// A dry run must say whether the IDENTICAL executing request would refuse
/// (issue #79): the plan's numbers alone read as a clean 200, and an
/// operator would learn about the force gate from the request that was
/// meant to do the work. Both triggers, on the report and on the status
/// route — one decision function, three askers.
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

    // (1) Loss: `gold` has no reading at all.
    let dry = match h
        .schema_admin
        .schema_repin("level", "severity", None, true, false)
        .await
        .expect("dry run")
    {
        RepinStart::Report(job) => job,
        other => panic!("expected a report, got {other:?}"),
    };
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

    // The status route is the same row, so it carries the same verdict.
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
    let dry = match h
        .schema_admin
        .schema_repin("pri", "severity", None, true, false)
        .await
        .expect("dry run")
    {
        RepinStart::Report(job) => job,
        other => panic!("expected a report, got {other:?}"),
    };
    assert_eq!(dry.projected_nulls, 0, "every value has an OTel reading");
    assert_eq!(dry.ambiguous_numerals, 1);
    assert_eq!(dry.requires_force, Some(true), "{dry:?}");
    let reason = dry.requires_force_reason.clone().expect("a reason");
    assert!(reason.contains("numeral 1-7"), "{reason}");
    assert!(reason.contains("dialect=syslog"), "{reason}");

    // Asserting syslog answers the ambiguity, so the same corpus needs no
    // force at all — and `--force` clears the OTel one.
    let dry = match h
        .schema_admin
        .schema_repin("pri", "severity", Some("syslog"), true, false)
        .await
        .expect("dry run")
    {
        RepinStart::Report(job) => job,
        other => panic!("expected a report, got {other:?}"),
    };
    assert_eq!(dry.dialect.as_deref(), Some("syslog"));
    assert_eq!(dry.ambiguous_numerals, 1, "the COUNT is dialect-blind");
    assert_eq!(
        dry.requires_force,
        Some(false),
        "an asserted dialect IS the answer to the ambiguity: {dry:?}"
    );
    let forced = match h
        .schema_admin
        .schema_repin("pri", "severity", None, true, true)
        .await
        .expect("dry run")
    {
        RepinStart::Report(job) => job,
        other => panic!("expected a report, got {other:?}"),
    };
    assert_eq!(
        forced.requires_force,
        Some(false),
        "force clears the gate: {forced:?}"
    );
}

/// The force verdict has THREE states, and the missing one is the one that
/// matters (issue #79 review): a claimed job whose scan has not recorded a
/// plan yet reports NO verdict. Its counts are zeros meaning "not measured",
/// and answering `false` there tells an operator polling the status route
/// that a job about to 409 is clean.
#[tokio::test(flavor = "multi_thread")]
async fn the_force_verdict_is_absent_until_the_scan_has_a_plan() {
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
            .schema_repin("level", "severity", None, true, false)
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
    trawl_server::repin::engine::TEST_SCAN_DELAY_MS.store(0, std::sync::atomic::Ordering::Relaxed);
    // …and once the plan exists the verdict is a fact, both on the report
    // and on the status route.
    assert_eq!(dry.requires_force, Some(true), "{dry:?}");
    let latest = h
        .schema_admin
        .schema_repin_status()
        .await
        .expect("status")
        .job
        .expect("a job has run");
    assert_eq!(latest.requires_force, Some(true));

    // The third state: a plan with nothing to accept (a BIGINT field to
    // VARCHAR, which is lossless by construction).
    let clean = match h
        .schema_admin
        .schema_repin("dur", "varchar", None, true, false)
        .await
        .expect("dry run")
    {
        RepinStart::Report(job) => job,
        other => panic!("expected a report, got {other:?}"),
    };
    assert_eq!(
        clean.requires_force,
        Some(false),
        "VARCHAR is the always-lossless target: {clean:?}"
    );
    assert_eq!(clean.requires_force_reason, None);
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

/// AC2/AC5: the dry run's numbers ARE the executed rewrite's, and the
/// report carries the evidence an operator decides on (which value cannot
/// be read, and whether anything is still writing the field).
#[tokio::test(flavor = "multi_thread")]
async fn repin_to_severity_dry_run_matches_the_executed_rewrite() {
    let h = harness().await;
    h.ingest_and_compact(&severity_corpus()).await;
    assert_eq!(h.pinned_type("level").await, "VARCHAR");
    assert_eq!(h.count("last=1h | stats count()").await, 5);

    let dry = match h
        .schema_admin
        .schema_repin("level", "severity", None, true, false)
        .await
        .expect("dry run")
    {
        RepinStart::Report(job) => job,
        other => panic!("expected a report, got {other:?}"),
    };
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
    // The evidence, not just the count.
    assert_eq!(dry.unmapped_samples, vec!["gold".to_owned()]);
    let live = dry.liveness.as_ref().expect("the sender just wrote");
    assert_eq!(live.service, "api");
    assert_eq!(
        dry.requires_force,
        Some(true),
        "loss AND ambiguity, neither accepted yet"
    );

    // The same plan, executed with force: the rewrite achieves exactly what
    // the scan projected.
    let started = match h
        .schema_admin
        .schema_repin("level", "severity", None, false, true)
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

/// AC3: the syslog assertion INVERTS the numeral, is persisted on the job
/// row, and reads back off the status route — the one dialect-changing
/// decision an operator can make about their own corpus.
#[tokio::test(flavor = "multi_thread")]
async fn a_syslog_repin_inverts_the_ladder_and_persists_the_assertion() {
    let h = harness().await;
    h.ingest_and_compact(&severity_corpus()).await;

    // The ambiguity gate does not fire under an explicit syslog assertion
    // (the other half of that gate is covered by
    // `a_dry_run_reports_the_force_verdict_it_would_hit`), but `gold` is
    // still a loss, so this run is forced for that reason.
    let started = match h
        .schema_admin
        .schema_repin("level", "severity", Some("syslog"), false, true)
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

/// AC6 + ruling 13: a value a PRIOR conform shelved comes back under the
/// new pin — including one whose `_raw` key still carries the sender's
/// original mixed-case spelling, which the case-variant fallback recovers
/// best-effort (the documented Unicode-`lower()`-vs-ASCII-fold edge).
#[tokio::test(flavor = "multi_thread")]
async fn resurrection_recovers_a_shelved_token_under_a_case_variant_key() {
    let h = harness().await;

    // First typed sight pins BIGINT…
    h.ingest_and_compact(&[event("api", &json!({"lvl": 17}))])
        .await;
    assert_eq!(h.pinned_type("lvl").await, "BIGINT");
    // …and a token batch conflicts: the value is nulled and lives on in
    // `_raw`, which holds the sender's original key spelling (`Lvl`),
    // because `_raw` is captured before the name fold.
    h.ingest_and_compact(&[event("api", &json!({"Lvl": "error"}))])
        .await;
    assert_eq!(
        h.count("lvl=* last=1h | stats count()").await,
        1,
        "the token was shelved by the BIGINT pin"
    );

    let dry = match h
        .schema_admin
        .schema_repin("lvl", "severity", None, true, true)
        .await
        .expect("dry run")
    {
        RepinStart::Report(job) => job,
        other => panic!("expected a report, got {other:?}"),
    };
    assert_eq!(
        dry.resurrectable, 1,
        "the shelved `error` is recoverable from _raw under a case-variant key"
    );
    assert_eq!(dry.projected_nulls, 0, "17 is already a ladder position");

    let started = match h
        .schema_admin
        .schema_repin("lvl", "severity", None, false, true)
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

/// After the repin, the sender's own field is on the ladder in every lane —
/// the cold parquet, the hot buffer a live event lands in, the pipeline's
/// `where`, and the in-memory matcher the live tail uses — and a
/// newly-ingested `"error"` conforms to 17 with no further operator action.
#[tokio::test(flavor = "multi_thread")]
async fn post_repin_severity_binds_in_every_lane_including_live_ingest() {
    let h = harness().await;
    h.ingest_and_compact(&severity_corpus()).await;
    let started = match h
        .schema_admin
        .schema_repin("level", "severity", None, false, true)
        .await
        .expect("execute")
    {
        RepinStart::Started(job) => job,
        other => panic!("expected started, got {other:?}"),
    };
    let done = h.wait_terminal(started.id).await;
    assert_eq!(done.status, "succeeded", "error: {:?}", done.error);

    // Cold: the rewritten corpus.
    assert_eq!(h.count("level>=warn last=1h | stats count()").await, 3);

    // Live ingest, uncompacted: the hot branch conforms through the same
    // catalog pin, so a brand-new `"error"` is 17 the moment it lands —
    // nothing about the sender changed.
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
    // The pipeline lane binds the same pin (ADR-0011).
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
        filter.matches_at(&event, &trawl_core::context::EvalContext::capture())
    };
    assert!(matches("level>=warn"), "the live tail binds the new pin");
    assert!(matches("level=error"));
    assert!(!matches("level=trace3"));

    // And once it compacts, the answer does not move.
    h.compact_tick().await;
    assert_eq!(h.count("level>=warn last=1h | stats count()").await, 4);
    assert_eq!(h.count("level=error last=1h | stats count()").await, 4);
}

/// Every shadow pass — the initial build and the catch-up passes that fold
/// in files compaction wrote meanwhile — rides the job's asserted dialect,
/// while ordinary compaction stays on the `OTel` reading.
///
/// That combination is exactly the discontinuity the CLI warns about, so it
/// is asserted rather than assumed: a `3` the catch-up rewrote reads as
/// syslog err, and the next `3` to arrive after the cutover reads as `OTel`
/// trace3.
#[tokio::test(flavor = "multi_thread")]
async fn a_catch_up_pass_rides_the_jobs_dialect_while_live_ingest_stays_otel() {
    use std::sync::atomic::Ordering;

    let h = harness().await;
    h.ingest_and_compact(&[event("api", &json!({"level": "error"}))])
        .await;
    assert_eq!(h.pinned_type("level").await, "VARCHAR");

    // A barrier, not just a delay: pass 0 takes its source snapshot and then
    // waits, so the file this test writes next provably did not exist when
    // the build enumerated its sources. Only a catch-up pass can carry it
    // into the shadow, which is the claim under test; a delay alone would
    // let pass 0 see the file, and the assertions below would hold even if
    // catch-up passes read the wrong dialect.
    trawl_server::repin::engine::TEST_SNAPSHOT_TAKEN.store(false, Ordering::SeqCst);
    trawl_server::repin::engine::TEST_RELEASE_BUILD.store(false, Ordering::SeqCst);
    trawl_server::repin::engine::TEST_BARRIER_FIRST_PASS.store(true, Ordering::SeqCst);

    let started = match h
        .schema_admin
        .schema_repin("level", "severity", Some("syslog"), false, true)
        .await
        .expect("execute")
    {
        RepinStart::Started(job) => job,
        other => panic!("expected started, got {other:?}"),
    };

    for _ in 0..600 {
        if trawl_server::repin::engine::TEST_SNAPSHOT_TAKEN.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        trawl_server::repin::engine::TEST_SNAPSHOT_TAKEN.load(Ordering::SeqCst),
        "the build never reached its first snapshot"
    );
    // Written by compaction under the old pin, after that snapshot.
    h.ingest_and_compact(&[event("api", &json!({"level": "3"}))])
        .await;
    trawl_server::repin::engine::TEST_RELEASE_BUILD.store(true, Ordering::SeqCst);

    let done = h.wait_terminal(started.id).await;
    assert_eq!(done.status, "succeeded", "error: {:?}", done.error);
    // The mid-build row survived the swap, which only a catch-up pass can
    // do: the cutover publishes the shadow wholesale, so a file the build
    // never folded in would be gone.
    assert_eq!(h.count("last=1h | stats count()").await, 2);

    // And it took the job's syslog reading (err), not OTel's — the pass that
    // folded it in rode the job's dialect, not the live default.
    assert_eq!(
        h.count("level=error last=1h | stats count()").await,
        2,
        "the token and the inverted numeral are both in the error band"
    );
    assert_eq!(h.count("level=trace3 last=1h | stats count()").await, 0);

    // Ordinary compaction after the cutover conforms at OTel — the
    // discontinuity the report's liveness warning exists to state.
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

// -- cancellation (#109) -----------------------------------------------------

use std::sync::atomic::{AtomicBool, Ordering};

use trawl_client::RepinCancel;
use trawl_server::repin::engine::{
    TEST_HOLD_AFTER_NO_RETURN, TEST_HOLD_AFTER_PROGRESS, TEST_HOLD_IN_SCAN, TEST_PAST_NO_RETURN,
    TEST_PROGRESS_PUBLISHED, TEST_RELEASE_CUTOVER, TEST_RELEASE_JOB, TEST_RELEASE_SCAN,
    TEST_SCAN_HELD,
};
use trawl_server::store::{RepinJob, RepinJobStatus};

/// Bounded wait for a barrier the engine publishes. It proves nothing on
/// its own: it turns "the job reached that point" into an ordering the
/// assertions after it can stand on, and fails loudly rather than letting a
/// test proceed past a barrier that was never reached.
async fn await_barrier(flag: &AtomicBool, what: &str) {
    for _ in 0..600 {
        if flag.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{what}");
}

/// Every live file under the data root, as (relative path, length, content
/// hash). The corpus a cancelled job must leave exactly as it found it.
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

/// One labelled counter's value out of a scrape body.
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

    /// Wait for the detached request path to record the cancel on the job
    /// row, and return the instant it wrote. Bounded.
    async fn await_cancel_request(&self, id: i64) -> chrono::DateTime<chrono::Utc> {
        for _ in 0..600 {
            if let Some(at) = self.repin_row(id).await.cancel_requested_at {
                return at;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the cancel request never reached the job row");
    }

    /// Run retention the way trawld runs it (a spawned loop on a one-second
    /// tick), and wait for a tick to publish the suppression gauge. The
    /// gauge is 1 for every tick that stands down, so a stranded marker or
    /// staging root shows up here as a timeout rather than a pass.
    async fn await_retention_gauge(&self) -> f64 {
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let _retention = trawl_server::retention::spawn_retention(
            self.data_dir.clone(),
            trawl_server::config::RetentionConfig {
                max_age_days: 90,
                min_free_disk_bytes: 0,
                retention_interval_secs: 1,
            },
            rx,
        );
        for _ in 0..200 {
            if let Some(value) = gauge_value(
                &scrape_metrics(&self.server.url).await,
                trawl_server::metrics::RETENTION_SUPPRESSED,
            ) {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("retention never published its suppression gauge");
    }
}

/// AC1: a cancel mid-build stops the job at the next file boundary and
/// leaves nothing behind: no staging, no marker, and a corpus identical
/// byte for byte to the one the job started from.
///
/// Held at the build's first published progress, so "mid-build" is an
/// ordering rather than a hope: the shadow generation exists, the marker is
/// on disk, and the job cannot terminalize until this test releases it.
/// The second cancel is the idempotence check. A repeat is accepted, and
/// the row still names the first request, timestamp included.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one cancel, and everything it must leave alone
async fn a_mid_build_cancel_leaves_the_corpus_and_the_staging_untouched() {
    let h = harness().await;
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

    TEST_PROGRESS_PUBLISHED.store(false, Ordering::SeqCst);
    TEST_RELEASE_JOB.store(false, Ordering::SeqCst);
    TEST_HOLD_AFTER_PROGRESS.store(true, Ordering::SeqCst);

    let started = match h
        .schema_admin
        .schema_repin("status", "VARCHAR", None, false, false)
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

    // Mid-build means there is something to unwind: the shadow generation
    // and the marker that licenses its deletion both exist right now.
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

    // Idempotent: a second asker is accepted and changes nothing.
    match h
        .schema_admin
        .schema_repin_cancel()
        .await
        .expect("second cancel")
    {
        RepinCancel::Cancelling(_) => {}
        other => panic!("a repeat request is accepted, got {other:?}"),
    }

    TEST_RELEASE_JOB.store(true, Ordering::SeqCst);
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

    // Nothing staged survives, and the marker went with it.
    assert!(!trawl_server::repin::shadow_root(&h.data_dir).exists());
    assert!(!trawl_server::repin::aside_root(&h.data_dir).exists());
    assert!(!trawl_server::repin::marker_path(&h.data_dir).exists());

    // The corpus is the one the job started from, file for file.
    assert_eq!(corpus_digest(&h.data_dir), before);
    assert_eq!(h.pinned_type("status").await, "BIGINT", "the pin stands");
    assert_eq!(h.count("last=1h | stats count()").await, 6);
    assert_eq!(h.count("status>=400 last=1h | stats count()").await, 3);

    // Metered exactly once, under the outcome the operator asked for.
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

    // And retention is free again: the sweep stands down for a marker or a
    // staging root, and the cancel left neither.
    let suppressed = h.await_retention_gauge().await;
    assert!(
        suppressed.abs() < f64::EPSILON,
        "a cancelled job must not leave retention suppressed: {suppressed}"
    );
}

/// AC2: a cancel during the mandatory scan stops at a file boundary, and
/// the job terminalizes without ever publishing a plan or touching the
/// disk. A dry run's own request answers 200 with the cancelled row, never
/// a fourth status code, since 409 already means refused-needs-force.
///
/// The scan is HELD at its first file rather than merely slowed: a scan
/// that finished first would record a plan, which is exactly what this
/// asserts never happened.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_during_the_scan_stops_before_any_plan_is_published() {
    let h = harness().await;
    let services = ["api", "web", "worker", "edge"];
    for svc in services {
        h.ingest_and_compact(&[event(svc, &json!({"status": 200}))])
            .await;
    }

    TEST_SCAN_HELD.store(false, Ordering::SeqCst);
    TEST_RELEASE_SCAN.store(false, Ordering::SeqCst);
    TEST_HOLD_IN_SCAN.store(true, Ordering::SeqCst);

    let client = h.schema_admin.clone();
    let dry = tokio::spawn(async move {
        client
            .schema_repin("status", "VARCHAR", None, true, false)
            .await
    });
    await_barrier(&TEST_SCAN_HELD, "the scan never reached a file boundary").await;

    // A scan has staged nothing, so there is nothing on disk to unwind.
    assert!(!trawl_server::repin::marker_path(&h.data_dir).exists());
    assert!(!trawl_server::repin::shadow_root(&h.data_dir).exists());

    match h.schema_admin.schema_repin_cancel().await.expect("cancel") {
        RepinCancel::Cancelling(_) => {}
        other => panic!("expected an accepted cancel, got {other:?}"),
    }
    TEST_RELEASE_SCAN.store(true, Ordering::SeqCst);

    let report = match dry
        .await
        .expect("join")
        .expect("a cancelled dry run answers with its row, not an error")
    {
        RepinStart::Report(job) => job,
        other => panic!("expected the cancelled row as a report, got {other:?}"),
    };
    assert_eq!(report.status, "cancelled");
    assert_eq!(report.cancelled_by.as_deref(), Some("schema-admin-key"));
    let error = report
        .error
        .clone()
        .expect("a cancelled row explains itself");
    assert!(error.contains("during scan"), "{error}");

    // No plan: the counts are the row's zeros meaning "not measured", and
    // `planned_at` is what says so.
    let row = h.repin_row(report.id).await;
    assert_eq!(row.planned_at, None, "no partial plan may be published");
    assert_eq!(report.files_total, 0);
    assert!(
        report.files_total < u64::try_from(services.len()).unwrap(),
        "the scan stopped short of the corpus"
    );

    // And the disk was never touched, before or after the stop.
    assert!(!trawl_server::repin::marker_path(&h.data_dir).exists());
    assert!(!trawl_server::repin::shadow_root(&h.data_dir).exists());
    assert!(!trawl_server::repin::aside_root(&h.data_dir).exists());
    assert_eq!(h.pinned_type("status").await, "BIGINT");
}

/// AC3: past the point of no return a cancel is refused, not queued. The
/// job latched before the Cutover marker went down, so there is nothing
/// left to unwind, and the refusal writes nothing to the row, because a
/// request that took no effect must not read as one that did.
///
/// Held between the marker write and the first env swap, the window the
/// refusal exists for; it is two renames wide in production, so a test
/// aiming at it by timing would be asserting on its own scheduler.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_past_the_point_of_no_return_is_refused_and_the_job_completes() {
    let h = harness().await;
    h.ingest_and_compact(&[
        event("api", &json!({"status": 200})),
        event("api", &json!({"status": 404})),
    ])
    .await;
    assert_eq!(h.pinned_type("status").await, "BIGINT");

    TEST_PAST_NO_RETURN.store(false, Ordering::SeqCst);
    TEST_RELEASE_CUTOVER.store(false, Ordering::SeqCst);
    TEST_HOLD_AFTER_NO_RETURN.store(true, Ordering::SeqCst);

    let started = match h
        .schema_admin
        .schema_repin("status", "VARCHAR", None, false, false)
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

    TEST_RELEASE_CUTOVER.store(true, Ordering::SeqCst);
    let done = h.wait_terminal(started.id).await;
    assert_eq!(done.status, "succeeded", "error: {:?}", done.error);
    assert_eq!(done.cancel_requested_at, None);
    assert_eq!(done.cancelled_by, None);

    // Forward: the corpus is the new generation and the pin agrees.
    assert_eq!(h.pinned_type("status").await, "VARCHAR");
    assert_eq!(h.count("last=1h | stats count()").await, 2);
    assert_eq!(h.count("status>=400 last=1h | stats count()").await, 1);
    h.wait_cleanup().await;
    assert!(!trawl_server::repin::shadow_root(&h.data_dir).exists());
    assert!(!trawl_server::repin::aside_root(&h.data_dir).exists());
    assert!(!trawl_server::repin::marker_path(&h.data_dir).exists());
}

/// AC4: a process that dies between an accepted cancel request and any
/// boundary observing it recovers as `failed`, never `cancelled`.
///
/// `cancelled` is a live-process word: it means a file boundary saw the
/// request and the unwind actually ran. Recovery cannot know that, and it
/// must not infer it from `cancel_requested_at`, so the request fields
/// survive as the audit trail they are, beside a `failed` verdict.
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
        requested_by: Some("op"),
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

    // Both halves of boot recovery, in the order a boot runs them.
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

    // The one-running slot is free: the next repin is claimed, not 409ed.
    let next = store.claim(claim).await.expect("the slot is free again");
    assert_ne!(next, job_id);
}

/// AC5: the audit trail an operator reads after the fact: the request, the
/// effect (with the stage it landed in), and the refusal, plus the one event
/// boot recovery must NOT emit.
///
/// One test rather than three: the capture layer is a global subscriber and
/// only the first installer in a process wins, so the three paths have to
/// share it.
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

    let h = harness().await;
    for svc in ["api", "web"] {
        h.ingest_and_compact(&[event(svc, &json!({"status": 200}))])
            .await;
    }

    // (1) A build-stage cancel: request, then effect.
    TEST_PROGRESS_PUBLISHED.store(false, Ordering::SeqCst);
    TEST_RELEASE_JOB.store(false, Ordering::SeqCst);
    TEST_HOLD_AFTER_PROGRESS.store(true, Ordering::SeqCst);
    let cancelled = match h
        .schema_admin
        .schema_repin("status", "VARCHAR", None, false, false)
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
    TEST_RELEASE_JOB.store(true, Ordering::SeqCst);
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

    // (2) A refusal past the point of no return.
    TEST_PAST_NO_RETURN.store(false, Ordering::SeqCst);
    TEST_RELEASE_CUTOVER.store(false, Ordering::SeqCst);
    TEST_HOLD_AFTER_NO_RETURN.store(true, Ordering::SeqCst);
    let completing = match h
        .schema_admin
        .schema_repin("status", "VARCHAR", None, false, false)
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
    TEST_RELEASE_CUTOVER.store(true, Ordering::SeqCst);
    assert_eq!(h.wait_terminal(completing.id).await.status, "succeeded");
    let refused = capture
        .await_event("repin_cancel_refused")
        .await
        .expect("the refusal is audited");
    assert!(refused.fields["actor"].contains("schema-admin-key"));
    assert!(refused.fields["job_id"].contains(&completing.id.to_string()));

    // (3) Boot recovery over a crash-after-request state emits no
    // `repin_cancelled`: nothing observed the request, so nothing may claim
    // the unwind ran.
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
            requested_by: Some("op"),
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

/// A tracing capture layer over the events this file asserts on.
///
/// Its own copy rather than a shared one: `tests/common` is compiled into
/// every integration binary in the crate, and a helper only this file uses
/// would be dead code in all the others.
mod audit_capture {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// One captured event: its fields, stringified.
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

        /// How many of `event_type` have been captured so far.
        pub fn count(&self, event_type: &str) -> usize {
            self.matching(event_type).len()
        }

        /// The first `event_type` captured, waiting a bounded while for it:
        /// the request audit is written by a detached task, so its arrival
        /// is ordered after the 202 rather than with it.
        pub async fn await_event(&self, event_type: &str) -> Option<Captured> {
            for _ in 0..200 {
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
