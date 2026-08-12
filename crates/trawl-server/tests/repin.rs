// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end repin tests (ADR-0011 slice B, issue #53), on the
//! catalog-surface harness: a real TLS server over a per-test data root,
//! ingest through HTTP, compaction driven per-tick with the catalog and
//! the repin coordinator wired exactly as trawld wires them.

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

async fn harness(pool: sqlx::PgPool) -> Harness {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let wal_dir = root.join("wal");
    let data_dir = root.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let data_glob = format!("{}/**/*.parquet", data_dir.display());

    let server = setup_in_dir_with_data(pool, &root, data_glob, RateLimitConfig::default()).await;
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
    /// One compaction tick with the catalog AND the repin coordinator
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

    /// The catalog's advertised type for a field, via `/api/v1/schema`.
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

/// THE acceptance test: a BIGINT-pinned field with a shelved conflict
/// value is repinned to VARCHAR. The dry run projects, the rewrite
/// matches the projection, existing queries answer identically, the
/// shelved value becomes queryable, unaffected files keep their inodes,
/// and a foreign file rides across untouched.
#[sqlx::test(migrations = false)]
async fn repin_is_invisible_to_queries_and_resurrects_shelved_values(pool: sqlx::PgPool) {
    let h = harness(pool).await;

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

    // A foreign parquet inside the env dir but OFF the layout: it must
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

    // Mandatory dry run: one affected file, one resurrectable value,
    // nothing lost (VARCHAR is the always-lossless target).
    let dry = match h
        .schema_admin
        .schema_repin("status", "varchar", true, false)
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
        .schema_repin("status", "VARCHAR", false, false)
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

    // The pin flipped, with no window of absent pin.
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
#[sqlx::test(migrations = false)]
async fn lossy_repin_refuses_without_force_and_accounts_with_it(pool: sqlx::PgPool) {
    let h = harness(pool).await;

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
        .schema_repin("dur", "BIGINT", false, false)
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
        .schema_repin("dur", "BIGINT", false, true)
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

/// The force gate is asked of the FINISHED shadow, not just the pre-build
/// scan: a lossless plan whose corpus grows a non-conforming value while
/// the rewrite runs is refused at the cutover, corpus untouched — and the
/// same repin proceeds under force.
#[sqlx::test(migrations = false)]
async fn late_arriving_loss_refuses_the_cutover_without_force(pool: sqlx::PgPool) {
    let h = harness(pool).await;

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
        .schema_repin("dur", "BIGINT", true, false)
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
        .schema_repin("dur", "BIGINT", false, false)
        .await
        .expect("execute")
    {
        RepinStart::Started(job) => job,
        other => panic!("expected started, got {other:?}"),
    };

    // A value the new pin cannot read, ingested and compacted DURING the
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
        .schema_repin("dur", "BIGINT", false, true)
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

/// One repin at a time: a concurrent second request 409s with the error
/// envelope (not a refusal plan); ingest and queries ride through the
/// slowed rewrite — events land in the hot buffer immediately and exactly
/// once in the post-cutover corpus, and a query loop across the whole job
/// (build, cutover, sweep) never errors.
#[sqlx::test(migrations = false)]
async fn ingest_queries_and_a_second_repin_ride_through_a_slow_rewrite(pool: sqlx::PgPool) {
    let h = harness(pool).await;

    // Seed a few affected files across services (more files = longer
    // build under the per-file delay).
    for svc in ["api", "web", "worker"] {
        h.ingest_and_compact(&[
            event(svc, &json!({"status": 200})),
            event(svc, &json!({"status": 404})),
        ])
        .await;
    }
    assert_eq!(h.count("last=1h | stats count()").await, 6);

    trawl_server::repin::engine::TEST_FILE_DELAY_MS
        .store(250, std::sync::atomic::Ordering::Relaxed);
    let started = match h
        .schema_admin
        .schema_repin("status", "VARCHAR", false, false)
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
        .schema_repin("status", "DOUBLE", false, false)
        .await;
    match second {
        Err(trawl_client::ClientError::Server { status, .. }) => assert_eq!(status, 409),
        other => panic!("expected a 409 error envelope, got {other:?}"),
    }

    // Ingest DURING the build: visible immediately via the hot buffer.
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

    // Query in a loop across the whole job: no query may ever error.
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
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
    };

    let done = h.wait_terminal(started.id).await;
    trawl_server::repin::engine::TEST_FILE_DELAY_MS.store(0, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(done.status, "succeeded", "error: {:?}", done.error);

    let queries = query_loop.await.expect("query loop");
    assert!(queries > 0, "the loop observed the running job");

    // Exactly once: everything ingested before and during the job.
    assert_eq!(h.pinned_type("status").await, "VARCHAR");
    assert_eq!(h.count("last=1h | stats count()").await, 7);
    assert_eq!(h.count("status=418 last=1h | stats count()").await, 1);

    // Progress metrics were scrapeable mid-job and the outcome landed.
    let metrics = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap()
        .get(format!("{}/metrics", h.server.url))
        .send()
        .await
        .expect("metrics scrape")
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("trawl_catalog_repin_jobs_total{outcome=\"succeeded\"}"),
        "outcome counter missing"
    );
    assert!(metrics.contains("trawl_catalog_repin_duration_seconds"));
}

/// The postgres half of boot recovery over a real storage state: a marker
/// in the cutover phase finishes the job transactionally, flips the pin,
/// re-arms the conformance pass, sweeps the aside, and removes the
/// marker; orphaned running rows without a marker fail.
#[sqlx::test(migrations = false)]
async fn boot_reconciliation_completes_a_recovered_cutover(pool: sqlx::PgPool) {
    let h = harness(pool).await;

    // A pinned custom field to flip.
    h.ingest_and_compact(&[event("api", &json!({"status": 200}))])
        .await;
    assert_eq!(h.pinned_type("status").await, "BIGINT");

    // Simulate a crash mid-cutover on a THROWAWAY root: job row claimed,
    // marker written in phase cutover, swap half-done.
    let job_id = h
        .server
        .state
        .storage
        .repin
        .claim(
            "status",
            trawl_core::schema::CanonicalType::BigInt,
            trawl_core::schema::CanonicalType::Varchar,
            false,
            false,
            None,
        )
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

    // An orphaned running row (no marker) fails at the same boot step.
    let orphan_id = h
        .server
        .state
        .storage
        .repin
        .claim(
            "status",
            trawl_core::schema::CanonicalType::Varchar,
            trawl_core::schema::CanonicalType::BigInt,
            false,
            false,
            None,
        )
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
#[sqlx::test(migrations = false)]
async fn resurrection_only_pass_recovers_without_retyping(pool: sqlx::PgPool) {
    let h = harness(pool).await;
    h.ingest_and_compact(&[event("api", &json!({"status": 200}))])
        .await;
    h.ingest_and_compact(&[event("api", &json!({"status": "accepted"}))])
        .await;
    assert_eq!(h.pinned_type("status").await, "BIGINT");

    // Same type without force is a 400 (nothing to do without intent).
    let err = h
        .schema_admin
        .schema_repin("status", "BIGINT", false, false)
        .await
        .expect_err("same-type without force refuses");
    match err {
        trawl_client::ClientError::Server { status, .. } => assert_eq!(status, 400),
        other => panic!("expected 400, got {other:?}"),
    }

    // With force: a resurrection-only rewrite. `"accepted"` still has no
    // BIGINT reading — it stays shelved and counts as nulled-projection
    // zero (it was ALREADY null) — but a recoverable value would return.
    let dry = match h
        .schema_admin
        .schema_repin("status", "BIGINT", true, true)
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
