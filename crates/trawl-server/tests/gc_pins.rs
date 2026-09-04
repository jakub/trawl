// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end pin garbage collection (#110): a real TLS server over a
//! per-test data root, pins taken by real ingest and compaction, and the
//! engine run both directly and over the route.
//!
//! Most tests build their own [`PinGc`] rather than reaching for
//! `state.gc`, because the retention floor is the one input the harness
//! cannot vary: the packaged default is 90 days, which floors every window
//! a test would otherwise ask for. The tests that go through the route (or
//! through the wired engine) therefore assert the floor and the report
//! rather than a deletion.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::{TestServer, setup_in_dir_with_data};
use serde_json::json;
use trawl_client::HttpClient;
use trawl_server::catalog::CatalogContext;
use trawl_server::catalog::gc::{GcActor, PinGc};
use trawl_server::config::RateLimitConfig;

const DAY: u64 = 86_400;

fn now_ts() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

fn catalog_ctx(server: &TestServer) -> CatalogContext {
    CatalogContext {
        store: server.state.storage.catalog.clone(),
        cache: server.state.query.field_catalog.clone(),
    }
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

/// An unlabelled counter's value, 0 when it has never been incremented
/// (prometheus does not render a counter until something touches it).
/// Counters render as integers, so a non-integer body is a rendering
/// change worth failing on rather than rounding past.
fn counter_value(scrape: &str, name: &str) -> u64 {
    scrape
        .lines()
        .find_map(|line| {
            let rest = line.strip_prefix(name)?.strip_prefix(' ')?;
            rest.trim().parse().ok()
        })
        .unwrap_or(0)
}

/// A gauge's value, `None` when nothing has published it. Gauges render as
/// f64, so this reads them that way rather than assuming an integer form.
fn gauge_value(scrape: &str, name: &str) -> Option<f64> {
    scrape.lines().find_map(|line| {
        let rest = line.strip_prefix(name)?.strip_prefix(' ')?;
        rest.trim().parse().ok()
    })
}

/// Every `*.parquet` under a directory tree.
fn walk_parquet(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "parquet") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

struct Harness {
    server: TestServer,
    wal_dir: PathBuf,
    data_dir: PathBuf,
    ingest: HttpClient,
    query: HttpClient,
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
    Harness {
        server,
        wal_dir,
        data_dir,
        ingest,
        query,
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
        let errors = trawl_server::ingest::compaction::compact_once_coordinated(
            &self.wal_dir,
            &self.data_dir,
            Duration::ZERO,
            false,
            Some(hot_buffer),
            500,
            "2GB",
            Some(&ctx),
            Some(self.coordinator()),
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

    fn coordinator(&self) -> &Arc<trawl_server::repin::RepinCoordinator> {
        self.server
            .state
            .ingest
            .repin_coordinator
            .as_ref()
            .expect("ingest-enabled server has a repin coordinator")
    }

    /// A gc engine over this server's catalog, with an explicit retention
    /// floor.
    fn gc(&self, retention_floor_secs: Option<u64>) -> Arc<PinGc> {
        Arc::new(PinGc::new(
            self.server.state.storage.catalog.clone(),
            self.server.state.storage.repin.clone(),
            Arc::clone(&self.server.state.query.field_catalog),
            Arc::clone(self.coordinator()),
            self.data_dir.clone(),
            retention_floor_secs,
        ))
    }

    /// Take a pin for `field` through the real ingest path, then delete the
    /// parquet the batch published: a pin whose data is gone, which is the
    /// shape gc exists for (a decommissioned sender whose files aged out).
    async fn pin_without_carrier(&self, service: &str, field: &str) {
        self.ingest_and_compact(&[event(service, &json!({ field: "v" }))])
            .await;
        let published = walk_parquet(&self.data_dir)
            .into_iter()
            .find(|p| {
                p.file_name()
                    .is_some_and(|n| n == format!("{service}.parquet").as_str())
            })
            .unwrap_or_else(|| panic!("compaction published {service}.parquet"));
        std::fs::remove_file(&published).expect("remove the published carrier");
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

    fn pinned(&self, field: &str) -> bool {
        self.server.state.query.field_catalog.get(field).is_some()
    }

    /// Whether postgres still holds the pin (the cache can lag by design;
    /// this is the authority).
    async fn pinned_in_store(&self, field: &str) -> bool {
        self.server
            .state
            .storage
            .catalog
            .load_pins()
            .await
            .expect("load pins")
            .into_iter()
            .any(|(name, _)| name == field)
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

/// The acceptance truth table over one synthetic corpus: a pin whose
/// column a standing footer declares survives however long it has gone
/// unobserved, a pin with neither observation nor carrier is reclaimed,
/// and the envelope is never even a candidate.
#[tokio::test(flavor = "multi_thread")]
async fn gc_deletes_only_pins_with_no_observation_and_no_carrier() {
    let h = harness().await;

    // A standing carrier: `kept` lives in a file nobody deleted.
    h.ingest_and_compact(&[event("api", &json!({"kept": 1}))])
        .await;
    // No carrier: `dead`'s only file is gone.
    h.pin_without_carrier("gone", "dead").await;
    assert!(h.pinned("kept") && h.pinned("dead"));

    // A zero window makes every observed pin old, so the footer axis is
    // the only thing standing between a pin and deletion.
    let report = h
        .gc(None)
        .run(Some(Duration::ZERO), false, GcActor::default())
        .await
        .expect("gc runs");

    assert_eq!(report.deleted, 1, "report: {report:?}");
    let names: Vec<&str> = report.candidates.iter().map(|c| c.field.as_str()).collect();
    assert_eq!(names, vec!["dead"]);
    assert!(report.files_scanned >= 1, "the corpus was walked");
    assert!(
        report.pins_examined >= 2,
        "both custom pins were candidates on the observation axis"
    );

    assert!(!h.pinned("dead"), "the reclaimed pin left the cache");
    assert!(!h.pinned_in_store("dead").await, "and left postgres");
    assert!(h.pinned("kept"), "a carried pin is never reclaimed");

    // The envelope is trawl's contract, not a reclaimable slot — and it is
    // filtered before the footer scan, so it never even appears as a
    // candidate.
    for envelope in ["_severity", "_time", "_raw", "host", "service", "message"] {
        assert!(
            !names.contains(&envelope),
            "{envelope} must never be a gc candidate"
        );
        assert!(h.pinned_in_store(envelope).await, "{envelope} pin survives");
    }
}

/// A pin something wrote inside the window stays, carrier or no carrier.
#[tokio::test(flavor = "multi_thread")]
async fn gc_retains_a_pin_observed_inside_the_window() {
    let h = harness().await;
    h.pin_without_carrier("gone", "fresh").await;

    let report = h
        .gc(None)
        .run(Some(Duration::from_secs(3600)), false, GcActor::default())
        .await
        .expect("gc runs");

    assert_eq!(report.deleted, 0, "report: {report:?}");
    assert!(report.candidates.is_empty());
    assert_eq!(
        report.files_scanned, 0,
        "an empty candidate set skips the walk entirely"
    );
    assert!(h.pinned("fresh"));
}

/// The retention floor raises a shorter request, and the report says so —
/// asserted through the engine `AppState` actually wires up, which reads
/// the packaged 90-day retention window.
#[tokio::test(flavor = "multi_thread")]
async fn gc_floors_a_short_request_at_the_retention_window() {
    let h = harness().await;
    h.pin_without_carrier("gone", "dead").await;

    let gc = h
        .server
        .state
        .gc
        .clone()
        .expect("an ingest node wires up a gc engine");
    let report = gc
        .run(
            Some(Duration::from_secs(7 * DAY)),
            false,
            GcActor::default(),
        )
        .await
        .expect("gc runs");

    assert_eq!(report.requested_older_than_secs, 7 * DAY);
    assert_eq!(report.retention_floor_secs, Some(90 * DAY));
    assert_eq!(
        report.effective_older_than_secs,
        90 * DAY,
        "a window shorter than the corpus trawl still keeps is raised"
    );
    assert_eq!(report.deleted, 0);
    assert!(
        h.pinned("dead"),
        "nothing is 90 days unobserved in a test that just started"
    );
}

/// An empty parquet still declares its schema, and the schema is the
/// evidence: a file with zero rows is a carrier.
#[tokio::test(flavor = "multi_thread")]
async fn gc_reads_an_empty_parquet_as_a_carrier() {
    let h = harness().await;
    h.ingest_and_compact(&[event("lonely", &json!({"rare": 1}))])
        .await;

    // Empty the published file in place, keeping its schema.
    let file = walk_parquet(&h.data_dir)
        .into_iter()
        .find(|p| p.file_name().is_some_and(|n| n == "lonely.parquet"))
        .expect("lonely.parquet exists");
    let emptied = file.with_extension("emptied");
    let conn = duckdb::Connection::open_in_memory().unwrap();
    conn.execute_batch(&format!(
        "COPY (SELECT * FROM read_parquet('{}') WHERE 1=0) TO '{}' (FORMAT PARQUET)",
        file.display(),
        emptied.display()
    ))
    .expect("write an empty parquet with the same schema");
    std::fs::rename(&emptied, &file).unwrap();
    assert_eq!(h.count("last=1h | stats count()").await, 0, "no rows left");

    let report = h
        .gc(None)
        .run(Some(Duration::ZERO), false, GcActor::default())
        .await
        .expect("gc runs");

    assert_eq!(report.deleted, 0, "report: {report:?}");
    assert!(h.pinned("rare"), "an empty file still names its columns");
}

/// Being wrong is survivable by construction: the purge is metadata only,
/// so a field that comes back simply gets pinned again and queries answer
/// across the old and new files.
#[tokio::test(flavor = "multi_thread")]
async fn gc_being_wrong_costs_only_a_clean_repin() {
    let h = harness().await;
    h.ingest_and_compact(&[event("api", &json!({"kept": 1}))])
        .await;
    h.pin_without_carrier("gone", "dur").await;

    let report = h
        .gc(None)
        .run(Some(Duration::ZERO), false, GcActor::default())
        .await
        .expect("gc runs");
    assert_eq!(report.deleted, 1);
    assert!(!h.pinned("dur"));

    // The sender returns. Compaction pins the field afresh and conforms
    // the batch to it, with no conflict and no error.
    h.ingest_and_compact(&[event("gone", &json!({"dur": 42}))])
        .await;
    assert_eq!(
        h.server.state.query.field_catalog.get("dur"),
        Some(trawl_core::schema::CanonicalType::BigInt),
        "the re-pin reads the wire value, not the old pin"
    );
    assert_eq!(h.count("dur=42 last=1h | stats count()").await, 1);

    // And a hot event on top of the cold file: the union still types.
    h.ingest
        .ingest(&[event("gone", &json!({"dur": 43}))])
        .await
        .expect("ingest");
    assert_eq!(h.count("dur>=42 last=1h | stats count()").await, 2);
}

/// The corpus gate is what makes the footer proof binding: while a
/// compaction batch holds the read side, gc cannot scan, so a file that
/// batch publishes is part of the corpus gc reads.
#[tokio::test(flavor = "multi_thread")]
async fn gc_waits_for_the_compaction_batch_and_sees_what_it_published() {
    let h = harness().await;
    h.ingest_and_compact(&[event("late", &json!({"slow": 1}))])
        .await;
    let carrier = walk_parquet(&h.data_dir)
        .into_iter()
        .find(|p| p.file_name().is_some_and(|n| n == "late.parquet"))
        .expect("late.parquet exists");
    // Unpublished: the batch has not landed it yet.
    let stashed = h.data_dir.join("late.parquet.stash");
    std::fs::rename(&carrier, &stashed).unwrap();

    // A batch in flight, exactly as compaction holds it around its
    // pin-snapshot → conform → publish phase.
    let batch = h.coordinator().compaction_guard().await;

    let gc = h.gc(None);
    let run = tokio::spawn(async move {
        gc.run(Some(Duration::ZERO), false, GcActor::default())
            .await
    });

    // The batch publishes its file while gc is held out. No sleep decides
    // this: gc cannot reach the write side of the gate until the guard
    // below is dropped, so the rename provably precedes the scan.
    std::fs::rename(&stashed, &carrier).unwrap();
    assert!(
        !run.is_finished(),
        "gc must not answer while a compaction batch holds the corpus gate"
    );

    drop(batch);
    let report = run.await.expect("gc task").expect("gc runs");
    assert_eq!(
        report.deleted, 0,
        "the freshly published carrier is evidence: {report:?}"
    );
    assert!(h.pinned("slow"));
}

/// One run at a time. A second caller is refused immediately rather than
/// queued behind a scan that holds the corpus gate — two operators on the
/// route must not add up to an ingest stall.
///
/// Deterministic by construction: the first run is held at the corpus gate
/// by a compaction batch, so it provably still owns the admission lock when
/// the second one asks.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_concurrent_gc_run_is_refused_immediately() {
    let h = harness().await;
    h.ingest_and_compact(&[event("api", &json!({"kept": 1}))])
        .await;
    h.pin_without_carrier("gone", "dead").await;
    let gc = h.gc(None);

    // A compaction batch holds the corpus gate, so the first run cannot get
    // past it and cannot release the admission lock.
    let batch = h.coordinator().compaction_guard().await;
    let first = {
        let gc = Arc::clone(&gc);
        tokio::spawn(async move {
            gc.run(Some(Duration::ZERO), false, GcActor::default())
                .await
        })
    };

    // The first run has to reach the gate before the admission lock is
    // observable; it cannot finish while the batch is held, so this loop
    // ends on the refusal rather than on a timer.
    let mut refusal = None;
    for _ in 0..200 {
        match gc.run(Some(Duration::ZERO), true, GcActor::default()).await {
            Err(e)
                if e.error_class() == "conflict"
                    && e.to_string().contains("already in progress") =>
            {
                refusal = Some(e);
                break;
            }
            other => {
                assert!(
                    !first.is_finished(),
                    "the first run answered while the corpus gate was held: {other:?}"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
    let refusal = refusal.expect("a second run must be refused while the first holds admission");
    assert!(refusal.to_string().contains("pin gc run"), "{refusal}");
    assert!(h.pinned("dead"), "a refused second run deletes nothing");

    drop(batch);
    let report = first
        .await
        .expect("gc task")
        .expect("the first run finishes");
    assert_eq!(report.deleted, 1, "report: {report:?}");

    // And the lock is released with the run: the next caller is admitted.
    let again = gc
        .run(Some(Duration::ZERO), true, GcActor::default())
        .await
        .expect("admission is free once the first run is done");
    assert!(again.candidates.is_empty(), "report: {again:?}");
}

/// The report and the audit name what postgres deleted, never what the walk
/// projected. A candidate whose row goes away underneath the run — a
/// concurrent purge, an operator with psql — is not a pin this run
/// reclaimed, and an audit record claiming otherwise is a lie in the one
/// place an operator has to trust.
#[tokio::test(flavor = "multi_thread")]
async fn the_report_names_only_the_pins_the_purge_returned() {
    let h = harness().await;
    h.ingest_and_compact(&[event("api", &json!({"kept": 1}))])
        .await;
    h.pin_without_carrier("gone", "dead").await;
    h.pin_without_carrier("also_gone", "deader").await;

    // Hold the run at the corpus gate: its candidate set is already read.
    let batch = h.coordinator().compaction_guard().await;
    let gc = h.gc(None);
    let run = {
        let gc = Arc::clone(&gc);
        tokio::spawn(async move {
            gc.run(Some(Duration::ZERO), false, GcActor::default())
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!run.is_finished(), "the batch holds the gate");

    // One candidate is reclaimed by somebody else while the run waits.
    h.server
        .state
        .storage
        .catalog
        .delete_pins(&["deader".to_owned()])
        .await
        .expect("a concurrent purge");

    drop(batch);
    let report = run.await.expect("gc task").expect("gc runs");

    assert_eq!(
        report.pins_examined, 4,
        "`deader` was still a candidate when the run read the observation \
         axis: {report:?}"
    );
    assert_eq!(
        report
            .candidates
            .iter()
            .map(|c| c.field.as_str())
            .collect::<Vec<_>>(),
        vec!["dead"],
        "`deader` was proved dead by the walk but deleted by somebody else, \
         so it is not this run's deletion: {report:?}"
    );
    assert_eq!(report.deleted, 1, "report: {report:?}");
    assert!(!h.pinned_in_store("deader").await, "it is gone either way");
}

/// Every way a repin can own the data root refuses the same way, and
/// deletes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_repin_in_flight_refuses_gc_four_ways() {
    let h = harness().await;
    // A standing file, so `dead` is the only pin without a carrier (every
    // event carries `timestamp` as ordinary sender vocabulary).
    h.ingest_and_compact(&[event("api", &json!({"kept": 1}))])
        .await;
    h.pin_without_carrier("gone", "dead").await;
    let gc = h.gc(None);

    let marker = trawl_server::repin::marker_path(&h.data_dir);
    let shadow = trawl_server::repin::shadow_root(&h.data_dir);
    let aside = trawl_server::repin::aside_root(&h.data_dir);

    for (what, install, remove) in [
        (
            "marker",
            Box::new(|| std::fs::write(&marker, b"{}").unwrap()) as Box<dyn Fn()>,
            Box::new(|| std::fs::remove_file(&marker).unwrap()) as Box<dyn Fn()>,
        ),
        (
            "shadow root",
            Box::new(|| std::fs::create_dir_all(&shadow).unwrap()),
            Box::new(|| std::fs::remove_dir_all(&shadow).unwrap()),
        ),
        (
            "aside root",
            Box::new(|| std::fs::create_dir_all(&aside).unwrap()),
            Box::new(|| std::fs::remove_dir_all(&aside).unwrap()),
        ),
    ] {
        install();
        let err = gc
            .run(Some(Duration::ZERO), false, GcActor::default())
            .await
            .expect_err("gc refuses while a repin owns the data root");
        assert_eq!(err.error_class(), "conflict", "{what}: {err}");
        assert!(err.to_string().contains("repin"), "{what}: {err}");
        assert!(h.pinned("dead"), "{what} refusal deleted a pin");
        remove();
    }

    // The fourth: a claimed job row with no filesystem evidence yet.
    let job = h
        .server
        .state
        .storage
        .repin
        .claim(trawl_server::store::RepinClaim {
            field: "dead",
            from_type: trawl_core::schema::CanonicalType::Varchar,
            to_type: trawl_core::schema::CanonicalType::BigInt,
            dialect: None,
            dry_run: false,
            force: false,
            requested_by: Some("test"),
        })
        .await
        .expect("claim a repin job");
    let err = gc
        .run(Some(Duration::ZERO), false, GcActor::default())
        .await
        .expect_err("a running repin job refuses gc");
    assert_eq!(err.error_class(), "conflict", "{err}");
    assert!(err.to_string().contains(&job.to_string()), "{err}");
    assert!(h.pinned("dead"));

    // With the job finished, the same request goes through.
    let finished = h
        .server
        .state
        .storage
        .repin
        .finish_if_running(
            job,
            trawl_server::store::RepinJobStatus::Failed,
            Some("test"),
        )
        .await
        .expect("finish the job");
    assert!(finished, "the claimed job was still running");
    let report = gc
        .run(Some(Duration::ZERO), false, GcActor::default())
        .await
        .expect("gc runs once nothing owns the corpus");
    assert_eq!(report.deleted, 1);
}

/// A file gc cannot read is a file whose columns are unknown, and an
/// unknown file cannot be part of a proof that nothing carries a field.
/// The whole run refuses and mutates nothing.
#[tokio::test(flavor = "multi_thread")]
async fn gc_refuses_the_whole_run_on_an_unreadable_parquet() {
    let h = harness().await;
    h.ingest_and_compact(&[event("api", &json!({"kept": 1}))])
        .await;
    h.pin_without_carrier("gone", "dead").await;
    let junk = h.data_dir.join("prod").join("junk.parquet");
    std::fs::create_dir_all(junk.parent().unwrap()).unwrap();
    std::fs::write(&junk, b"PAR1 and then some lies").unwrap();

    let err = h
        .gc(None)
        .run(Some(Duration::ZERO), false, GcActor::default())
        .await
        .expect_err("an unreadable parquet fails the run closed");

    assert_eq!(err.error_class(), "conflict", "{err}");
    let msg = err.to_string();
    assert!(msg.contains("junk.parquet"), "the refusal names it: {msg}");
    assert!(msg.contains("deleted nothing"), "{msg}");
    assert!(h.pinned("dead"), "a refused run deletes nothing");
    assert!(h.pinned_in_store("dead").await);

    // Move the offending file aside and the same request succeeds — the
    // refusal was about the corpus, not about the candidate.
    std::fs::remove_file(&junk).unwrap();
    let report = h
        .gc(None)
        .run(Some(Duration::ZERO), false, GcActor::default())
        .await
        .expect("gc runs over a readable corpus");
    assert_eq!(report.deleted, 1);
}

/// A data root that is not there is UNKNOWN, never an empty corpus. Read as
/// empty, no footer would disprove anything and the run would reclaim every
/// candidate pin in the catalog — an unmounted volume or a mistyped
/// `data_dir` turned into a mass delete.
#[tokio::test(flavor = "multi_thread")]
async fn gc_refuses_a_data_root_that_is_not_there() {
    let h = harness().await;
    // A standing carrier, so `dead` is the only pin without one.
    h.ingest_and_compact(&[event("api", &json!({"kept": 1}))])
        .await;
    h.pin_without_carrier("gone", "dead").await;

    let vanished = h.data_dir.parent().unwrap().join("not-mounted");
    let gc = Arc::new(PinGc::new(
        h.server.state.storage.catalog.clone(),
        h.server.state.storage.repin.clone(),
        Arc::clone(&h.server.state.query.field_catalog),
        Arc::clone(h.coordinator()),
        vanished.clone(),
        None,
    ));

    let err = gc
        .run(Some(Duration::ZERO), false, GcActor::default())
        .await
        .expect_err("a missing data root fails the run closed");
    assert_eq!(err.error_class(), "conflict", "{err}");
    let msg = err.to_string();
    assert!(
        msg.contains("not-mounted"),
        "the refusal names the root: {msg}"
    );
    assert!(msg.contains("deleted nothing"), "{msg}");
    assert!(h.pinned("dead"), "a refused run deletes nothing");
    assert!(h.pinned_in_store("dead").await);

    // The same catalog over the real root reclaims exactly the dead pin, so
    // the refusal was about the corpus and not about the candidate.
    let report = h
        .gc(None)
        .run(Some(Duration::ZERO), false, GcActor::default())
        .await
        .expect("gc runs over the real data root");
    assert_eq!(report.deleted, 1, "report: {report:?}");
}

/// A symlink under a live env is refused for the same reason: its target
/// is a file trawl does not own.
#[tokio::test(flavor = "multi_thread")]
async fn gc_refuses_the_whole_run_on_a_symlink_under_a_live_env() {
    let h = harness().await;
    h.pin_without_carrier("gone", "dead").await;
    let link = h.data_dir.join("prod").join("elsewhere.parquet");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/nowhere/at/all.parquet", &link).unwrap();

    let err = h
        .gc(None)
        .run(Some(Duration::ZERO), false, GcActor::default())
        .await
        .expect_err("a symlink under a live env fails the run closed");
    assert_eq!(err.error_class(), "conflict", "{err}");
    assert!(err.to_string().contains("symlink"), "{err}");
    assert!(h.pinned("dead"));
}

/// A `.parquet` that is not a regular file is refused, not opened. Opening
/// a FIFO blocks until somebody writes the other end, and the walk runs
/// with the corpus gate held, so that is an ingest stall with no timeout on
/// it. The unix socket here stands in for the whole class (a FIFO, a device
/// node): all of them fail the same lstat check.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn gc_refuses_a_parquet_that_is_not_a_regular_file() {
    let h = harness().await;
    h.ingest_and_compact(&[event("api", &json!({"kept": 1}))])
        .await;
    h.pin_without_carrier("gone", "dead").await;

    let odd = h.data_dir.join("prod").join("not-a-file.parquet");
    std::fs::create_dir_all(odd.parent().unwrap()).unwrap();
    let _listener = std::os::unix::net::UnixListener::bind(&odd).expect("bind a socket file");

    let run = tokio::time::timeout(
        Duration::from_secs(30),
        h.gc(None)
            .run(Some(Duration::ZERO), false, GcActor::default()),
    )
    .await
    .expect("the run must answer promptly rather than block on the open");
    let err = run.expect_err("a non-regular parquet fails the run closed");

    assert_eq!(err.error_class(), "conflict", "{err}");
    let msg = err.to_string();
    assert!(msg.contains("not-a-file.parquet"), "{msg}");
    assert!(msg.contains("not a regular file"), "{msg}");
    assert!(h.pinned("dead"), "a refused run deletes nothing");
    assert!(h.pinned_in_store("dead").await);

    // Removed, the same request goes through.
    std::fs::remove_file(&odd).unwrap();
    let report = h
        .gc(None)
        .run(Some(Duration::ZERO), false, GcActor::default())
        .await
        .expect("gc runs over a corpus of ordinary files");
    assert_eq!(report.deleted, 1, "report: {report:?}");
}

mod capture {
    //! Minimal tracing capture: every event's fields as strings.

    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Debug)]
    pub struct Captured {
        pub fields: BTreeMap<String, String>,
    }

    #[derive(Clone, Default)]
    pub struct Capture {
        events: Arc<Mutex<Vec<Captured>>>,
    }

    impl Capture {
        /// Every captured event carrying `event_type = ty`.
        pub fn of_type(&self, ty: &str) -> Vec<Captured> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter(|e| {
                    e.fields
                        .get("event_type")
                        .is_some_and(|t| t.contains(ty) && t.len() == ty.len() + 2)
                })
                .cloned()
                .collect()
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

/// The dry run is the safety, so it must mutate nothing at all: no rows,
/// no cache generation, no metric, no audit record. Execution then emits
/// exactly one `catalog_pin_gc` per pin it deleted, and a second run over
/// the same corpus is a no-op.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one narrative: dry run, execution, repeat
async fn gc_dry_run_mutates_nothing_and_execution_audits_every_deleted_pin() {
    use tracing_subscriber::prelude::*;

    let events = capture::Capture::default();
    let subscriber = tracing_subscriber::registry().with(events.clone().with_filter(
        tracing_subscriber::EnvFilter::new(trawl_server::telemetry::DEFAULT_LOG_FILTER),
    ));
    // Global, not thread-local: the run happens on another tokio worker.
    tracing::subscriber::set_global_default(subscriber).expect("no prior global subscriber");

    let h = harness().await;
    h.ingest_and_compact(&[event("api", &json!({"kept": 1}))])
        .await;
    h.pin_without_carrier("gone", "dead").await;
    h.pin_without_carrier("also_gone", "deader").await;

    let gc = h.gc(None);
    let cache = &h.server.state.query.field_catalog;
    let generation_before = cache.repin_generation();
    let before_scrape = scrape_metrics(&h.server.url).await;
    let metric_before = counter_value(&before_scrape, "trawl_catalog_pins_gc_total");
    let pinned_gauge_before = gauge_value(&before_scrape, "trawl_catalog_pinned_fields")
        .expect("ingest published the fill gauge");

    let dry = gc
        .run(Some(Duration::ZERO), true, GcActor::default())
        .await
        .expect("dry run");
    let would_delete: Vec<&str> = dry.candidates.iter().map(|c| c.field.as_str()).collect();
    assert_eq!(would_delete, vec!["dead", "deader"], "report: {dry:?}");
    assert!(dry.dry_run);
    assert_eq!(dry.deleted, 0);
    assert!(h.pinned_in_store("dead").await && h.pinned_in_store("deader").await);
    assert_eq!(
        cache.repin_generation(),
        generation_before,
        "a dry run does not invalidate a reader's cached pin set"
    );
    assert_eq!(
        counter_value(
            &scrape_metrics(&h.server.url).await,
            "trawl_catalog_pins_gc_total"
        ),
        metric_before
    );
    assert!(
        events.of_type("catalog_pin_gc").is_empty(),
        "a dry run deletes nothing, so it records no deletion"
    );

    let actor = GcActor {
        name: Some("schema-admin-key".to_owned()),
        key_prefix: Some("k_abc123".to_owned()),
    };
    let real = gc
        .run(Some(Duration::ZERO), false, actor)
        .await
        .expect("execution");
    assert_eq!(real.deleted, 2, "report: {real:?}");
    assert_eq!(
        real.candidates
            .iter()
            .map(|c| c.field.as_str())
            .collect::<Vec<_>>(),
        would_delete,
        "the dry run projected exactly what the execution did"
    );
    assert!(!h.pinned_in_store("dead").await && !h.pinned_in_store("deader").await);
    assert!(h.pinned_in_store("kept").await);
    assert_eq!(
        cache.repin_generation(),
        generation_before + 1,
        "one purge, one generation bump"
    );
    let after_purge = scrape_metrics(&h.server.url).await;
    assert_eq!(
        counter_value(&after_purge, "trawl_catalog_pins_gc_total"),
        metric_before + 2
    );
    // The fill gauge follows the eviction, published from the count the
    // purge transaction returned. Nothing fallible sits between the commit
    // and the eviction, so the operator's headroom reflects the reclaim as
    // soon as the run answers.
    assert_eq!(
        gauge_value(&after_purge, "trawl_catalog_pinned_fields"),
        Some(pinned_gauge_before - 2.0),
        "the reclaim must show up in the fill gauge"
    );

    let audited = events.of_type("catalog_pin_gc");
    assert_eq!(audited.len(), 2, "one record per deleted pin: {audited:?}");
    let fields: Vec<&str> = audited.iter().map(|e| e.fields["field"].as_str()).collect();
    assert_eq!(fields, vec!["dead", "deader"]);
    for record in &audited {
        assert_eq!(record.fields["actor"], "\"schema-admin-key\"");
        assert_eq!(record.fields["actor_key_prefix"], "\"k_abc123\"");
        assert_eq!(record.fields["deleted_type"], "VARCHAR");
        assert!(record.fields.contains_key("pinned_at"));
        assert!(record.fields.contains_key("last_seen"));
        assert_eq!(record.fields["effective_older_than_secs"], "0");
    }
    let summary = events.of_type("catalog_pin_gc_complete");
    assert_eq!(summary.len(), 2, "one per run, dry included: {summary:?}");
    assert!(summary.last().unwrap().fields.contains_key("gate_held_ms"));
    assert!(summary.last().unwrap().fields.contains_key("files_scanned"));

    // A repeat run has nothing left to reclaim.
    let again = gc
        .run(Some(Duration::ZERO), false, GcActor::default())
        .await
        .expect("repeat run");
    assert_eq!(again.deleted, 0, "report: {again:?}");
    assert!(again.candidates.is_empty());
    assert_eq!(events.of_type("catalog_pin_gc").len(), 2, "no new records");
    assert_eq!(
        counter_value(
            &scrape_metrics(&h.server.url).await,
            "trawl_catalog_pins_gc_total"
        ),
        metric_before + 2,
        "a run that deleted nothing increments nothing"
    );
    assert_eq!(
        cache.repin_generation(),
        generation_before + 1,
        "and evicts nothing"
    );
    assert_eq!(
        events.of_type("catalog_pin_gc_complete").len(),
        3,
        "but still says what it did"
    );
}

/// The route reports the server's own three window numbers, and a
/// `schema_write` key is what reaches it. This one goes through the real
/// wired engine, so the packaged 90-day retention floor applies: nothing
/// in a freshly started test is 90 days unobserved, and the honest answer
/// is a report with no candidates.
#[tokio::test(flavor = "multi_thread")]
async fn gc_pins_route_reports_the_window_the_server_decided() {
    let h = harness().await;
    h.pin_without_carrier("gone", "dead").await;
    let ops = HttpClient::new_insecure(&h.server.url, &h.server.schema_admin_token).unwrap();

    let dry = ops
        .schema_gc_pins(true, Some(7 * DAY))
        .await
        .expect("dry run over the route");
    assert!(dry.dry_run);
    assert_eq!(dry.requested_older_than_secs, 7 * DAY);
    assert_eq!(dry.retention_floor_secs, Some(90 * DAY));
    assert_eq!(dry.effective_older_than_secs, 90 * DAY);
    assert_eq!(dry.deleted, 0);
    assert!(dry.candidates.is_empty(), "report: {dry:?}");
    assert!(
        chrono::DateTime::parse_from_rfc3339(&dry.decided_at).is_ok(),
        "decided_at must be RFC 3339: {}",
        dry.decided_at
    );

    // The default window is the server's, not the client's: an omitted
    // `older_than_secs` still comes back floored and named.
    let real = ops
        .schema_gc_pins(false, None)
        .await
        .expect("execute over the route");
    assert!(!real.dry_run);
    assert_eq!(real.requested_older_than_secs, 30 * DAY);
    assert_eq!(real.effective_older_than_secs, 90 * DAY);
    assert_eq!(real.deleted, 0);
    assert!(h.pinned("dead"), "nothing was 90 days unobserved");
}

/// The engine's refusals reach the wire as 409s carrying the server's own
/// sentence, because that sentence is the operator's instruction.
#[tokio::test(flavor = "multi_thread")]
async fn gc_pins_route_surfaces_a_refusal_as_a_409() {
    let h = harness().await;
    h.pin_without_carrier("gone", "dead").await;
    let ops = HttpClient::new_insecure(&h.server.url, &h.server.schema_admin_token).unwrap();

    let marker = trawl_server::repin::marker_path(&h.data_dir);
    std::fs::write(&marker, b"{}").unwrap();
    let err = ops
        .schema_gc_pins(true, Some(0))
        .await
        .expect_err("a repin owning the data root refuses gc");
    match err {
        trawl_client::ClientError::Server { status, error } => {
            assert_eq!(status, 409);
            assert!(error.message.contains("repin"), "{error:?}");
        }
        other => panic!("expected a 409, got {other:?}"),
    }
    assert!(h.pinned("dead"), "a refusal deletes nothing");

    // Cleared, the same request is served again.
    std::fs::remove_file(&marker).unwrap();
    ops.schema_gc_pins(true, Some(0))
        .await
        .expect("the route works once nothing owns the corpus");
}
