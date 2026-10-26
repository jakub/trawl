// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Boot reconciliation and the sweep of captured results (ADR-0057), and
//! the evidence whose investigation's retention ended (ADR-0059), over a
//! booted fixture: a real results root, real rows in the fixture's
//! app-state database, and the fixture's lifecycle clock.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use common::captured::{self, Restartable};
use reqwest::StatusCode;
use serde_json::json;
use trawl_api::investigations::{CreateInvestigation, EntryPayload, InvestigationId};
use trawl_api::results::{
    CaptureKind, ColumnFacts, ResultColumn, ResultShape, ResultSource, StoredAs,
};
use trawl_api::value::ColumnType;
use trawl_server::config::{CaptureConfig, RateLimitConfig};
use trawl_server::results::ResultsState;
use trawl_server::results::charge::CaptureGate;
use trawl_server::results::root::{Published, ResultsRoot};
use trawl_server::results::sweep::{Reconciled, Sweep, reconcile_at_boot, spawn_sweep, sweep};
use trawl_server::store::{
    CaptureInsert, CreateOutcome, NewCapture, NewInvestigation, RetentionRule, StoreActor,
};

const KEY: i64 = 7;
const OTHER_KEY: i64 = 8;

fn id(n: u32) -> String {
    format!("{n:032x}")
}

/// A temporary one-column capture `id` of `key_id`, made at `created_at`
/// and expiring `ttl_secs` later, describing `file` or no file at all.
fn capture(
    id: &str,
    key_id: i64,
    created_at: DateTime<Utc>,
    ttl_secs: u64,
    file: Option<Published>,
) -> NewCapture {
    NewCapture {
        id: id.to_owned(),
        key_id,
        created_at,
        expires_at: created_at + TimeDelta::seconds(i64::try_from(ttl_secs).unwrap()),
        query: "_service=nginx".into(),
        executed_query: "_service=nginx | sort -_time, -_ingested".into(),
        source: ResultSource::Live,
        shape: ResultShape::Events,
        earliest: Some(created_at - TimeDelta::minutes(15)),
        latest: Some(created_at),
        columns: vec![ResultColumn {
            name: "_time".into(),
            ty: ColumnType::Timestamp,
            stored_as: StoredAs::Native,
        }],
        column_facts: vec![ColumnFacts {
            nulls: 0,
            distinct: 0,
            distinct_capped: false,
            values: None,
            min: None,
            max: None,
        }],
        captured_rows: 0,
        total_rows: 0,
        wire_bytes: 0,
        file_bytes: file.map_or(0, |f| f.file_bytes),
        digest: file.map(|f| f.digest),
        format_version: 1,
        kind: CaptureKind::Search,
        correlation: None,
        investigation_id: None,
    }
}

/// Publish `bytes` as result `id` in `root`.
fn publish(root: &ResultsRoot, id: &str, bytes: &[u8]) -> Published {
    let staged = root.stage().unwrap();
    std::fs::write(staged.path(), bytes).unwrap();
    root.publish(staged, id).unwrap()
}

/// Record `row` in the server's store.
async fn insert(results: &ResultsState, row: &NewCapture) {
    assert_eq!(
        results.store.insert(row).await.unwrap(),
        CaptureInsert::Inserted
    );
}

/// Publish a file for result `id` and record its row, made at `created_at`.
async fn stored(
    results: &ResultsState,
    id: &str,
    key_id: i64,
    created_at: DateTime<Utc>,
) -> Published {
    let published = publish(&results.root, id, id.as_bytes());
    let row = capture(
        id,
        key_id,
        created_at,
        results.capture.ttl_secs,
        Some(published),
    );
    insert(results, &row).await;
    published
}

/// The server's results state with other `[capture]` limits: the same
/// root, store and clock.
fn with_limits(results: &ResultsState, capture: CaptureConfig) -> ResultsState {
    ResultsState {
        root: Arc::clone(&results.root),
        store: results.store.clone(),
        gate: CaptureGate::new(
            &capture,
            results.store.clone(),
            Arc::clone(&results.root),
            results.clock.clone(),
        ),
        capture,
        investigation: results.investigation.clone(),
        clock: results.clock.clone(),
        in_flight: Arc::clone(&results.in_flight),
        steps: results.steps.clone(),
    }
}

/// The row's state and `gone_at`, read straight from the table, or `None`
/// once it is deleted.
async fn state_of(
    server: &common::TestServer,
    id: &str,
) -> Option<(String, Option<DateTime<Utc>>)> {
    let pool = common::fixture_pool(&server.app_db_url, 1).await;
    let row = sqlx::query_as("SELECT state, gone_at FROM captured_results WHERE id = $1")
        .bind(id)
        .fetch_optional(&pool)
        .await
        .unwrap();
    pool.close().await;
    row
}

fn exists(root: &ResultsRoot, id: &str) -> bool {
    root.path().join(format!("{id}.parquet")).exists()
}

fn seconds(secs: u64) -> TimeDelta {
    TimeDelta::seconds(i64::try_from(secs).unwrap())
}

/// The writer of every investigation here.
fn agent() -> StoreActor {
    StoreActor {
        key_id: KEY,
        prefix: "AbCd_-12".into(),
        name: "agent".into(),
    }
}

/// Create investigation `n` at `created`, its evidence due `due` later and
/// its ceiling 30 days after `created`.
async fn investigation(
    server: &common::TestServer,
    n: u32,
    created: DateTime<Utc>,
    due: TimeDelta,
) -> InvestigationId {
    let id = InvestigationId::parse(&id(n)).unwrap();
    let outcome = server
        .state
        .storage
        .investigations
        .create_if_absent(&NewInvestigation {
            id: id.clone(),
            actor: agent(),
            creation: CreateInvestigation {
                title: "502s".into(),
                question: "Why does checkout return 502?".into(),
                scope: String::new(),
            },
            now: created,
            retain_until: created + due,
            retain_ceiling: created + TimeDelta::days(30),
        })
        .await
        .unwrap();
    assert!(matches!(outcome, CreateOutcome::Created(_)), "{outcome:?}");
    id
}

/// Publish a file for result `id` and record it as kept evidence of
/// `investigation`, made at `created_at`.
async fn kept(
    results: &ResultsState,
    id: &str,
    investigation: &InvestigationId,
    created_at: DateTime<Utc>,
) {
    let published = publish(&results.root, id, id.as_bytes());
    let row = NewCapture {
        investigation_id: Some(investigation.clone()),
        ..capture(id, KEY, created_at, 0, Some(published))
    };
    assert_eq!(
        results.store.insert_kept_unlogged(&row).await.unwrap(),
        CaptureInsert::Inserted
    );
}

/// Plant `name` in the results root at `dir/data.results`, made before the
/// fixture boots over `dir`.
fn plant(dir: &Path, name: &str) -> std::path::PathBuf {
    let root = dir.join("data.results");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join(name);
    std::fs::write(&path, b"left by an earlier trawld").unwrap();
    path
}

/// Both boot doors reconcile: on an ingest node and on a query-only node,
/// a staged file and a published file no row names are gone before the
/// server answers, and an entry trawld does not make is left alone.
#[tokio::test]
async fn boot_removes_stages_and_files_no_capture_names() {
    for ingest in [true, false] {
        let tmp = tempfile::tempdir().unwrap();
        let stage = plant(tmp.path(), ".stage-AbCdEfGh01234567.parquet");
        let orphan = plant(tmp.path(), &format!("{}.parquet", id(1)));
        let foreign = plant(tmp.path(), "notes.txt");

        let server = common::setup_in_dir_with_ingest(tmp.path(), ingest).await;

        assert!(!stage.exists(), "ingest={ingest}: the stage survived boot");
        assert!(
            !orphan.exists(),
            "ingest={ingest}: the orphan survived boot"
        );
        assert!(foreign.exists(), "ingest={ingest}: a foreign entry went");
        assert_eq!(
            server.state.results.root.path(),
            tmp.path().join("data.results")
        );
    }
}

/// A live row whose file is gone is marked `file_missing`; a row with no
/// file by design (zero columns) and a row whose file is there are not.
#[tokio::test]
async fn boot_marks_live_rows_whose_files_are_gone() {
    let server = common::setup().await;
    let results = &server.state.results;
    let now = server.results_clock.now();
    let gone = capture(
        &id(10),
        KEY,
        now,
        results.capture.ttl_secs,
        Some(Published {
            file_bytes: 42,
            digest: [7; 32],
        }),
    );
    let fileless = capture(&id(11), KEY, now, results.capture.ttl_secs, None);
    insert(results, &gone).await;
    insert(results, &fileless).await;
    stored(results, &id(12), KEY, now).await;

    let reconciled = reconcile_at_boot(results).await.unwrap();
    assert_eq!(
        reconciled,
        Reconciled {
            missing: 1,
            ..Reconciled::default()
        }
    );
    let missing = |n| async move {
        results
            .store
            .get(&id(n), KEY)
            .await
            .unwrap()
            .unwrap()
            .file_missing
    };
    assert!(missing(10).await);
    assert!(!missing(11).await);
    assert!(!missing(12).await);
    assert!(exists(&results.root, &id(12)));
    // Marked once: a second boot changes nothing.
    assert_eq!(
        reconcile_at_boot(results).await.unwrap(),
        Reconciled::default()
    );
}

/// A temporary row already past its deadline when the server boots (it was
/// down when the row came due) expires at once, with `gone_at` at its
/// deadline and its file unlinked; a row still inside its lifetime is left.
#[tokio::test]
async fn boot_expires_rows_already_past_their_deadline() {
    let server = common::setup().await;
    let results = &server.state.results;
    let now = server.results_clock.now();
    let ttl = results.capture.ttl_secs;
    let created = now - seconds(ttl) - TimeDelta::minutes(5);
    stored(results, &id(15), KEY, created).await;
    stored(results, &id(16), KEY, now).await;

    let reconciled = reconcile_at_boot(results).await.unwrap();
    assert_eq!(
        reconciled,
        Reconciled {
            expired: 1,
            ..Reconciled::default()
        }
    );
    assert_eq!(
        state_of(&server, &id(15)).await,
        Some(("expired".to_owned(), Some(created + seconds(ttl))))
    );
    assert!(!exists(&results.root, &id(15)));
    assert_eq!(state_of(&server, &id(16)).await.unwrap().0, "temporary");
    assert!(exists(&results.root, &id(16)));
}

/// A key over a lowered `per_key_results` loses its oldest temporary
/// captures, rows pruned at the clock's instant and files unlinked; another
/// key under the quota keeps everything.
#[tokio::test]
async fn boot_prunes_the_oldest_captures_of_a_key_over_its_quota() {
    let server = common::setup().await;
    let now = server.results_clock.now();
    let results = with_limits(
        &server.state.results,
        CaptureConfig {
            per_key_results: 2,
            ..CaptureConfig::default()
        },
    );
    for (n, minutes) in [(20, 3), (21, 2), (22, 1)] {
        stored(&results, &id(n), KEY, now - TimeDelta::minutes(minutes)).await;
    }
    stored(&results, &id(30), OTHER_KEY, now - TimeDelta::minutes(10)).await;

    let reconciled = reconcile_at_boot(&results).await.unwrap();
    assert_eq!(
        reconciled,
        Reconciled {
            pruned: 1,
            ..Reconciled::default()
        }
    );
    assert_eq!(
        state_of(&server, &id(20)).await,
        Some(("pruned".to_owned(), Some(now)))
    );
    assert!(!exists(&results.root, &id(20)));
    for n in [21, 22, 30] {
        assert_eq!(
            state_of(&server, &id(n)).await.unwrap().0,
            "temporary",
            "{n}"
        );
        assert!(exists(&results.root, &id(n)), "{n}");
    }
}

/// An installation over a lowered `installation_bytes` loses its oldest
/// temporary captures across keys.
#[tokio::test]
async fn boot_prunes_the_installation_s_oldest_captures_across_keys() {
    let server = common::setup().await;
    let now = server.results_clock.now();
    // Each file holds its 32-byte id.
    let results = with_limits(
        &server.state.results,
        CaptureConfig {
            installation_bytes: 64,
            ..CaptureConfig::default()
        },
    );
    stored(&results, &id(40), KEY, now - TimeDelta::minutes(3)).await;
    stored(&results, &id(41), OTHER_KEY, now - TimeDelta::minutes(2)).await;
    stored(&results, &id(42), KEY, now - TimeDelta::minutes(1)).await;

    let reconciled = reconcile_at_boot(&results).await.unwrap();
    assert_eq!(reconciled.pruned, 1);
    assert_eq!(state_of(&server, &id(40)).await.unwrap().0, "pruned");
    assert!(!exists(&results.root, &id(40)));
    for n in [41, 42] {
        assert!(exists(&results.root, &id(n)), "{n}");
    }
}

/// On the fixture's clock: nothing happens before the deadline; at
/// `created + ttl` the row expires, with `gone_at` at the deadline, and its
/// file is unlinked; the tombstone stays until `expiry + tombstone` and is
/// deleted then.
#[tokio::test]
async fn sweep_expires_at_the_deadline_and_deletes_the_tombstone_after_its_lifetime() {
    let server = common::setup().await;
    let results = &server.state.results;
    let clock = &server.results_clock;
    let created = clock.now();
    stored(results, &id(50), KEY, created).await;
    let expiry = created + seconds(results.capture.ttl_secs);
    let deleted = expiry + seconds(results.capture.tombstone_secs);
    let micro = TimeDelta::microseconds(1);

    clock.set(expiry - micro);
    assert_eq!(sweep(results, clock.now()).await, Sweep::default());
    assert!(exists(&results.root, &id(50)));
    assert_eq!(state_of(&server, &id(50)).await.unwrap().0, "temporary");

    clock.set(expiry);
    assert_eq!(
        sweep(results, clock.now()).await,
        Sweep {
            expired: 1,
            unlinked: 1,
            ..Sweep::default()
        }
    );
    assert!(!exists(&results.root, &id(50)));
    assert_eq!(
        state_of(&server, &id(50)).await,
        Some(("expired".to_owned(), Some(expiry)))
    );

    clock.set(deleted - micro);
    assert_eq!(sweep(results, clock.now()).await, Sweep::default());
    assert!(state_of(&server, &id(50)).await.is_some());

    clock.set(deleted);
    assert_eq!(
        sweep(results, clock.now()).await,
        Sweep {
            tombstones: 1,
            ..Sweep::default()
        }
    );
    assert_eq!(state_of(&server, &id(50)).await, None);
}

/// A read that found a capture past its deadline marked it expired (and
/// unlinked its file); the sweep does not expire it again.
#[tokio::test]
async fn a_row_expired_lazily_by_a_read_is_not_expired_twice() {
    let server = common::setup().await;
    let results = &server.state.results;
    let created = server.results_clock.now();
    stored(results, &id(60), KEY, created).await;
    let after = created + seconds(results.capture.ttl_secs);

    assert!(results.store.mark_expired(&id(60), after).await.unwrap());
    assert!(results.root.unlink(&id(60)).unwrap());
    assert_eq!(sweep(results, after).await, Sweep::default());
    assert_eq!(
        state_of(&server, &id(60)).await,
        Some(("expired".to_owned(), Some(after)))
    );
}

/// A published file no row names is removed only once it is older than the
/// capture deadline, which covers a capture between publish and commit; a
/// file a live row names and a staged file are never touched.
#[tokio::test]
async fn sweep_removes_unnamed_files_only_past_the_capture_deadline() {
    let server = common::setup().await;
    let results = &server.state.results;
    let now = server.results_clock.now();
    let written = std::time::SystemTime::from(now);
    publish(&results.root, &id(70), b"published, never committed");
    std::fs::File::options()
        .write(true)
        .open(results.root.path_of(&id(70)).unwrap())
        .unwrap()
        .set_modified(written)
        .unwrap();
    stored(results, &id(71), KEY, now).await;
    std::fs::File::options()
        .write(true)
        .open(results.root.path_of(&id(71)).unwrap())
        .unwrap()
        .set_modified(written - std::time::Duration::from_secs(3600))
        .unwrap();
    let staged = results.root.stage().unwrap();
    let deadline = seconds(results.capture.deadline_secs);

    let early = sweep(results, now + deadline - TimeDelta::seconds(1)).await;
    assert_eq!(early, Sweep::default());
    assert!(exists(&results.root, &id(70)));

    let due = sweep(results, now + deadline).await;
    assert_eq!(
        due,
        Sweep {
            orphans: 1,
            ..Sweep::default()
        }
    );
    assert!(!exists(&results.root, &id(70)));
    assert!(exists(&results.root, &id(71)));
    assert!(
        staged.path().exists(),
        "an in-flight stage is not the sweep's"
    );
}

/// A capture whose row is still committing keeps its file through a sweep:
/// the file is published, the row's insert is held, and a sweep past the
/// capture deadline by the file's own age runs meanwhile. The insert then
/// commits, the capture answers, and its first page reads its rows.
#[tokio::test(flavor = "multi_thread")]
async fn a_sweep_leaves_the_file_of_a_capture_whose_row_is_committing() {
    let server = captured::boot(RateLimitConfig::default(), CaptureConfig::default()).await;
    server
        .results_clock
        .set(captured::at("2024-01-16T10:30:00Z"));
    let results = &server.state.results;
    let hold = results.store.faults().hold_next_insert();
    let body = json!({
        "query": format!("service={}", captured::SERVICE),
        "window": { "last": "1h" },
    });
    // Past the deadline by the file's modification time, which the system
    // clock stamps: the age floor alone would let the sweep unlink it.
    let late = Utc::now() + seconds(results.capture.deadline_secs) + TimeDelta::hours(1);

    let fixture = &server;
    let ((status, _, answer), (published, swept)) = tokio::join!(
        captured::capture(fixture, &fixture.analyst_token, &body),
        async move {
            hold.parked
                .await
                .expect("the capture reaches its row's insert");
            let published = captured::root_names(fixture);
            let swept = sweep(results, late).await;
            hold.release.send(()).unwrap();
            (published, swept)
        }
    );

    assert_eq!(status, StatusCode::OK, "{answer}");
    let id = answer["result"]["id"].as_str().unwrap();
    assert_eq!(
        published,
        [format!("{id}.parquet")],
        "the file was published before its row"
    );
    assert_eq!(swept, Sweep::default(), "the sweep left the capture alone");
    assert!(exists(&results.root, id), "the file is still there");
    let (status, _, page) = captured::page(&server, &server.analyst_token, id, "").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["pagination"]["returned"], json!(captured::ROWS));
}

/// A failed unlink is logged and counted, and the sweep's other steps
/// still run: here an expired capture's name holds a directory, and a
/// tombstone past its lifetime is deleted all the same.
#[tokio::test]
async fn a_failed_unlink_is_counted_and_the_sweep_goes_on() {
    let server = common::setup().await;
    let results = &server.state.results;
    let created = server.results_clock.now();
    let ttl = results.capture.ttl_secs;
    let tombstone = results.capture.tombstone_secs;
    // A capture long gone, still a tombstone.
    let old = created - seconds(ttl + tombstone) - TimeDelta::seconds(1);
    stored(results, &id(80), KEY, old).await;
    assert!(results.store.mark_expired(&id(80), created).await.unwrap());
    assert!(results.root.unlink(&id(80)).unwrap());
    // A capture due now whose name a directory took.
    let due = capture(
        &id(81),
        KEY,
        created - seconds(ttl),
        ttl,
        Some(Published {
            file_bytes: 5,
            digest: [1; 32],
        }),
    );
    insert(results, &due).await;
    std::fs::create_dir(results.root.path().join(format!("{}.parquet", id(81)))).unwrap();

    let outcome = sweep(results, created).await;
    assert_eq!(
        outcome,
        Sweep {
            expired: 1,
            tombstones: 1,
            errors: 1,
            ..Sweep::default()
        }
    );
    assert_eq!(state_of(&server, &id(80)).await, None);
    assert_eq!(state_of(&server, &id(81)).await.unwrap().0, "expired");
}

/// The scheduler's retention tick runs the sweep. An hourly poll makes the
/// retention interval one tick, and an interval's first tick is immediate,
/// so the spawned scheduler sweeps at once.
#[tokio::test]
async fn the_scheduler_s_retention_tick_sweeps_captured_results() {
    let server = common::setup().await;
    let results = &server.state.results;
    let now = server.results_clock.now();
    stored(
        results,
        &id(90),
        KEY,
        now - seconds(results.capture.ttl_secs),
    )
    .await;

    let (stop, stopped) = tokio::sync::watch::channel(false);
    let scheduler = trawl_server::scheduler::spawn_scheduler(
        server.state.storage.schedule.clone(),
        fleet_auth::KeyStore::from_pool(server.fleet_pool.clone()),
        server.state.query.pool.clone(),
        Arc::clone(&server.state.results),
        trawl_server::config::SchedulerConfig {
            poll_interval_secs: 3600,
            ..trawl_server::config::SchedulerConfig::default()
        },
        common::DEFAULT_TEST_TIMEOUT_SECS,
        stopped,
    );
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    while state_of(&server, &id(90)).await.unwrap().0 != "expired" {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the retention tick never swept"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(!exists(&results.root, &id(90)));
    stop.send(true).unwrap();
    scheduler.await.unwrap();
}

/// With no scheduler to carry the retention tick, the standalone sweep task
/// expires a capture past its deadline within its own interval, and stops
/// when told to.
#[tokio::test]
async fn the_standalone_sweep_tick_expires_captures_without_the_scheduler() {
    let server = common::setup().await;
    let results = &server.state.results;
    let clock = &server.results_clock;
    let created = clock.now();
    stored(results, &id(60), KEY, created).await;
    let expiry = created + seconds(results.capture.ttl_secs);
    clock.set(expiry + TimeDelta::seconds(1));

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let task = spawn_sweep(Arc::clone(results), Duration::from_secs(1), shutdown_rx);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if state_of(&server, &id(60)).await.map(|(state, _)| state) == Some("expired".to_owned()) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the sweep task never expired the capture"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        state_of(&server, &id(60)).await,
        Some(("expired".to_owned(), Some(expiry)))
    );
    assert!(!exists(&results.root, &id(60)));

    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the sweep task did not stop on shutdown")
        .unwrap();
}

/// The sweep ends kept evidence when its investigation's `retain_until`
/// passes: not a microsecond before, then once, the row `pruned` at the
/// deadline it passed and its file unlinked. The pruned row is evidence a
/// finding still cites, so it never ages out as a tombstone, and a second
/// sweep prunes nothing.
#[tokio::test]
async fn sweep_prunes_kept_evidence_once_its_investigation_s_retention_ends() {
    let server = common::setup().await;
    let results = &server.state.results;
    let clock = &server.results_clock;
    let created = clock.now();
    let due = created + TimeDelta::hours(1);
    let inv = investigation(&server, 1, created, TimeDelta::hours(1)).await;
    let later = investigation(&server, 2, created, TimeDelta::days(7)).await;
    kept(results, &id(100), &inv, created).await;
    kept(results, &id(101), &later, created).await;
    let micro = TimeDelta::microseconds(1);

    assert_eq!(sweep(results, due - micro).await, Sweep::default());
    assert_eq!(state_of(&server, &id(100)).await.unwrap().0, "kept");
    assert!(exists(&results.root, &id(100)));

    assert_eq!(
        sweep(results, due).await,
        Sweep {
            evidence_pruned: 1,
            unlinked: 1,
            ..Sweep::default()
        }
    );
    assert_eq!(
        state_of(&server, &id(100)).await,
        Some(("pruned".to_owned(), Some(due)))
    );
    assert!(!exists(&results.root, &id(100)));
    assert_eq!(state_of(&server, &id(101)).await.unwrap().0, "kept");
    assert!(exists(&results.root, &id(101)));

    let past_tombstone = due + seconds(results.capture.tombstone_secs) + TimeDelta::seconds(1);
    assert_eq!(sweep(results, past_tombstone).await, Sweep::default());
    assert_eq!(
        state_of(&server, &id(100)).await,
        Some(("pruned".to_owned(), Some(due))),
        "pruned evidence aged out as a tombstone"
    );
}

/// Evidence whose retention ended while the server was down is pruned at
/// boot, its file unlinked, before any capture can be admitted; evidence
/// still inside its retention stays.
#[tokio::test]
async fn boot_prunes_kept_evidence_whose_retention_ended_while_down() {
    let server = common::setup().await;
    let results = &server.state.results;
    let now = server.results_clock.now();
    let created = now - TimeDelta::days(8);
    let ended = investigation(&server, 1, created, TimeDelta::days(7)).await;
    let live = investigation(&server, 2, now, TimeDelta::days(7)).await;
    kept(results, &id(110), &ended, created).await;
    kept(results, &id(111), &live, now).await;

    assert_eq!(
        reconcile_at_boot(results).await.unwrap(),
        Reconciled {
            evidence_pruned: 1,
            ..Reconciled::default()
        }
    );
    assert_eq!(
        state_of(&server, &id(110)).await,
        Some(("pruned".to_owned(), Some(created + TimeDelta::days(7))))
    );
    assert!(!exists(&results.root, &id(110)));
    assert_eq!(state_of(&server, &id(111)).await.unwrap().0, "kept");
    assert!(exists(&results.root, &id(111)));
    assert_eq!(
        reconcile_at_boot(results).await.unwrap(),
        Reconciled::default()
    );
}

/// A step whose process died before it recorded an outcome reads
/// `interrupted` after the next boot, which advanced the sequence and left
/// the evidence clock where the step's admission put it.
#[tokio::test(flavor = "multi_thread")]
async fn boot_interrupts_the_steps_a_dead_process_left() {
    let (fixture, server) =
        Restartable::boot(RateLimitConfig::default(), CaptureConfig::default(), |_| {}).await;
    let now = server.results_clock.now();
    let inv = investigation(&server, 1, now, TimeDelta::days(7)).await;
    let store = &server.state.storage.investigations;
    let step = EntryPayload::Step {
        executed: "_service=checkout status>=500".into(),
        bounds: None,
        request_id: "r1".into(),
        query_id: None,
        purpose: None,
        outcome: None,
    };
    let rule = RetentionRule {
        idle: TimeDelta::days(7),
    };
    store
        .begin_step(
            &inv,
            &agent(),
            &step,
            now,
            rule,
            trawl_server::deadline::Deadline::after(std::time::Duration::from_mins(1)),
        )
        .await
        .unwrap();
    let before = store.get(&inv).await.unwrap().unwrap();
    server.results_clock.set(now + TimeDelta::days(1));

    let server = fixture.restart(server).await;
    let store = &server.state.storage.investigations;
    let after = store.get(&inv).await.unwrap().unwrap();
    assert_eq!(after.seq, before.seq + 1);
    assert_eq!(after.retain_until, before.retain_until);
    let page = store.read_since(&inv, 0, 10).await.unwrap().unwrap();
    let EntryPayload::Step { outcome, .. } = &page.entries[0].payload else {
        panic!("not a step: {:?}", page.entries[0]);
    };
    assert_eq!(
        outcome.as_ref().map(|o| o.outcome),
        Some(trawl_api::investigations::StepOutcomeKind::Interrupted)
    );
}
