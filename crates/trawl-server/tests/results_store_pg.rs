// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres tests for the captured-result metadata store (ADR-0057).
//!
//! Plain `#[sqlx::test]` applies `crates/trawl-server/migrations/` to each
//! per-test database. Every case runs against a fixed `NOW`, which the store
//! takes as a parameter: nothing here depends on the wall clock.

use chrono::{DateTime, Duration, TimeZone as _, Utc};
use sqlx::PgPool;
use trawl_api::investigations::{CreateInvestigation, InvestigationId};
use trawl_api::results::{
    CaptureKind, ColumnFacts, Correlation, FieldSightings, ResultColumn, ResultShape, ResultSource,
    ResultState, SightingGroup, SightingsDenominators, SightingsFacts, StoredAs, ValueCount,
};
use trawl_api::value::{ColumnType, Value};
use trawl_server::store::{
    CaptureInsert, CaptureRow, CreateOutcome, ExpiredFile, FactsDocument, InvestigationStore,
    NewCapture, NewInvestigation, ResultsStore, StoreActor, StoreError, Usage,
};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 23, 10, 0, 0).unwrap()
}

fn id(n: u32) -> String {
    format!("{n:032x}")
}

/// A live, temporary, one-column capture made `age` before `now()` by
/// `key_id`, expiring 15 minutes after it was made.
fn capture(n: u32, key_id: i64, age: Duration, file_bytes: u64) -> NewCapture {
    let created_at = now() - age;
    NewCapture {
        id: id(n),
        key_id,
        created_at,
        expires_at: created_at + Duration::minutes(15),
        query: "_service=nginx last=1h".into(),
        executed_query: "_service=nginx earliest=\"2026-10-23T09:00:00.000000Z\" \
                         latest=\"2026-10-23T10:00:00.000000Z\" | sort -_time, -_ingested"
            .into(),
        source: ResultSource::Live,
        shape: ResultShape::Events,
        earliest: Some(created_at - Duration::hours(1)),
        latest: Some(created_at),
        columns: vec![ResultColumn {
            name: "_time".into(),
            ty: ColumnType::Timestamp,
            stored_as: StoredAs::Native,
        }],
        column_facts: vec![ColumnFacts {
            nulls: 0,
            distinct: 1,
            distinct_capped: false,
            values: None,
            min: Some(Value::String("2026-10-23T09:30:00.000000Z".into())),
            max: Some(Value::String("2026-10-23T09:30:00.000000Z".into())),
        }],
        captured_rows: 1,
        total_rows: 1,
        wire_bytes: 30,
        file_bytes,
        digest: (file_bytes > 0).then_some([n.to_le_bytes()[0]; 32]),
        format_version: 1,
        kind: CaptureKind::Search,
        correlation: None,
        investigation_id: None,
    }
}

async fn insert(store: &ResultsStore, row: &NewCapture) {
    assert_eq!(store.insert(row).await.unwrap(), CaptureInsert::Inserted);
}

/// The investigation [`keep`] attaches rows to.
fn investigation_id() -> InvestigationId {
    InvestigationId::parse("feedfacefeedfacefeedfacefeedface").unwrap()
}

/// Keep a row for [`investigation_id`], creating that investigation on
/// first use. Written directly, so a row of any age or owner can be kept;
/// `promote` and its conditions have their own tests.
async fn keep(pool: &PgPool, id: &str) {
    let created = InvestigationStore::new(pool.clone())
        .create_if_absent(&NewInvestigation {
            id: investigation_id(),
            actor: StoreActor {
                key_id: 7,
                prefix: "AbCd_-12".into(),
                name: "agent".into(),
            },
            creation: CreateInvestigation {
                title: "t".into(),
                question: "q".into(),
                scope: String::new(),
            },
            now: now(),
            retain_until: now() + Duration::days(7),
            retain_ceiling: now() + Duration::days(30),
        })
        .await
        .unwrap();
    assert!(!matches!(created, CreateOutcome::Mismatch), "{created:?}");
    sqlx::query(
        "UPDATE captured_results SET state = 'kept', expires_at = NULL, investigation_id = $2
         WHERE id = $1",
    )
    .bind(id)
    .bind(investigation_id().as_str())
    .execute(pool)
    .await
    .unwrap();
}

async fn state_of(pool: &PgPool, id: &str) -> (String, Option<DateTime<Utc>>) {
    sqlx::query_as("SELECT state, gone_at FROM captured_results WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The constraint a refused insert names.
fn violated(err: StoreError) -> String {
    match err {
        StoreError::Unavailable(sqlx::Error::Database(db)) => {
            assert_eq!(db.code().as_deref(), Some("23514"), "{db}");
            db.constraint().expect("a named constraint").to_owned()
        }
        other => panic!("expected a database error, got {other:?}"),
    }
}

fn ids(files: &[ExpiredFile]) -> Vec<&str> {
    files.iter().map(|file| file.id.as_str()).collect()
}

#[sqlx::test]
async fn insert_and_get_round_trip_every_column(pool: PgPool) {
    let store = ResultsStore::new(pool.clone());
    let mut row = capture(1, 7, Duration::seconds(30), 65_536);
    row.columns.push(ResultColumn {
        name: "status".into(),
        ty: ColumnType::list(ColumnType::Integer),
        stored_as: StoredAs::WireText,
    });
    row.column_facts.push(ColumnFacts {
        nulls: 1,
        distinct: 2,
        distinct_capped: false,
        values: Some(vec![
            ValueCount {
                value: Value::UInt(u64::MAX),
                count: 4000,
            },
            ValueCount {
                value: Value::String("é\"\n".into()),
                count: 999,
            },
        ]),
        min: None,
        max: None,
    });
    row.captured_rows = 5000;
    row.total_rows = 20_000;
    row.digest = Some(*b"0123456789abcdef0123456789abcdef");
    // Microsecond precision survives every timestamp column.
    let micros = Duration::microseconds(250_001);
    row.created_at += micros;
    row.expires_at += micros;
    row.earliest = row.earliest.map(|at| at + micros);
    row.latest = row.latest.map(|at| at + micros);
    insert(&store, &row).await;

    let got = store.get(&row.id, 7).await.unwrap().expect("the row");
    assert_eq!(got.id, row.id);
    assert_eq!(got.key_id, 7);
    assert_eq!(got.created_at, row.created_at);
    assert_eq!(got.expires_at, Some(row.expires_at));
    assert_eq!(got.gone_at, None);
    assert_eq!(got.state, ResultState::Temporary);
    assert_eq!(got.query, row.query);
    assert_eq!(got.executed_query, row.executed_query);
    assert_eq!(got.source, ResultSource::Live);
    assert_eq!(got.shape, ResultShape::Events);
    assert_eq!(got.earliest, row.earliest);
    assert_eq!(got.latest, row.latest);
    assert_eq!(got.columns, row.columns);
    assert_eq!(got.column_facts, row.column_facts);
    assert_eq!(got.captured_rows, 5000);
    assert_eq!(got.total_rows, 20_000);
    assert_eq!(got.wire_bytes, 30);
    assert_eq!(got.file_bytes, 65_536);
    assert_eq!(got.digest, row.digest);
    assert!(!got.file_missing);
    assert_eq!(got.format_version, 1);
    assert_eq!(got.investigation_id, None);

    let wire = got.to_wire();
    assert_eq!(wire.created_at, "2026-10-23T09:59:30.250001Z");
    assert_eq!(
        wire.expires_at.as_deref(),
        Some("2026-10-23T10:14:30.250001Z")
    );
    assert_eq!(
        wire.earliest.as_deref(),
        Some("2026-10-23T08:59:30.250001Z")
    );
    assert_eq!(wire.latest.as_deref(), Some("2026-10-23T09:59:30.250001Z"));
    assert!(wire.truncated);
    assert_eq!(wire.columns, row.columns);
    assert_eq!(wire.column_facts, row.column_facts);
}

/// A stored source has no bounds, and a capture with no columns has no
/// file (D3): both read back as absent, and a kept row has no deadline.
#[sqlx::test]
async fn a_stored_zero_column_capture_round_trips_without_bounds_or_file(pool: PgPool) {
    let store = ResultsStore::new(pool.clone());
    let row = NewCapture {
        source: ResultSource::Stored,
        shape: ResultShape::Whole,
        earliest: None,
        latest: None,
        columns: vec![],
        column_facts: vec![],
        captured_rows: 0,
        total_rows: 0,
        wire_bytes: 0,
        ..capture(2, 7, Duration::zero(), 0)
    };
    assert_eq!(row.digest, None);
    insert(&store, &row).await;
    keep(&pool, &row.id).await;

    let got = store.get(&row.id, 7).await.unwrap().expect("the row");
    assert_eq!(got.state, ResultState::Kept);
    assert_eq!(
        (got.earliest, got.latest, got.expires_at),
        (None, None, None)
    );
    assert_eq!((got.file_bytes, got.digest), (0, None));
    assert_eq!(got.investigation_id, Some(investigation_id()));
    let wire = got.to_wire();
    assert_eq!(wire.investigation_id, Some(investigation_id()));
    assert_eq!(
        (wire.earliest, wire.latest, wire.expires_at),
        (None, None, None)
    );
    assert!(!wire.truncated);
}

#[sqlx::test]
async fn a_duplicate_id_is_refused_and_writes_nothing(pool: PgPool) {
    let store = ResultsStore::new(pool.clone());
    let first = capture(3, 7, Duration::zero(), 10);
    insert(&store, &first).await;

    let second = NewCapture {
        key_id: 8,
        query: "other".into(),
        ..capture(3, 8, Duration::zero(), 20)
    };
    assert_eq!(store.insert(&second).await.unwrap(), CaptureInsert::IdTaken);
    let kept = store
        .get(&first.id, 7)
        .await
        .unwrap()
        .expect("the first row");
    assert_eq!((kept.query, kept.file_bytes), (first.query, 10));
    assert_eq!(store.get(&first.id, 8).await.unwrap(), None);
}

#[sqlx::test]
async fn get_answers_only_the_owner(pool: PgPool) {
    let store = ResultsStore::new(pool.clone());
    let row = capture(4, 7, Duration::zero(), 10);
    insert(&store, &row).await;
    assert!(store.get(&row.id, 7).await.unwrap().is_some());
    assert_eq!(store.get(&row.id, 8).await.unwrap(), None);
    assert_eq!(store.get(&id(5), 7).await.unwrap(), None);
}

/// `get` answers every state, so the service can tell gone from absent.
#[sqlx::test]
async fn get_answers_a_row_in_any_state(pool: PgPool) {
    let store = ResultsStore::new(pool.clone());
    let row = capture(6, 7, Duration::minutes(20), 10);
    insert(&store, &row).await;
    assert!(store.mark_expired(&row.id, now()).await.unwrap());
    let got = store.get(&row.id, 7).await.unwrap().expect("the tombstone");
    assert_eq!(got.state, ResultState::Expired);
    assert_eq!(got.to_wire().state, ResultState::Expired);
}

/// The list is the owner's temporary recovery list: a kept result is
/// reached through its investigation.
#[sqlx::test]
async fn list_is_newest_first_and_live_temporary_only(pool: PgPool) {
    let store = ResultsStore::new(pool.clone());
    let old = capture(10, 7, Duration::minutes(10), 10);
    let new = capture(11, 7, Duration::minutes(1), 10);
    let due = capture(12, 7, Duration::minutes(15), 10); // expires_at == now
    let expired = capture(13, 7, Duration::minutes(30), 10);
    let kept = capture(14, 7, Duration::hours(5), 10);
    let foreign = capture(15, 8, Duration::minutes(1), 10);
    for row in [&old, &new, &due, &expired, &kept, &foreign] {
        insert(&store, row).await;
    }
    assert!(store.mark_expired(&expired.id, now()).await.unwrap());
    keep(&pool, &kept.id).await;

    let listed: Vec<String> = store
        .list(7, now())
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.id)
        .collect();
    assert_eq!(listed, [new.id, old.id]);
}

#[sqlx::test]
async fn mark_expired_sets_gone_at_to_the_deadline_and_is_idempotent(pool: PgPool) {
    let store = ResultsStore::new(pool.clone());
    let due = capture(20, 7, Duration::minutes(20), 10);
    let live = capture(21, 7, Duration::minutes(5), 10);
    insert(&store, &due).await;
    insert(&store, &live).await;

    assert!(store.mark_expired(&due.id, now()).await.unwrap());
    assert_eq!(
        state_of(&pool, &due.id).await,
        ("expired".to_owned(), Some(due.expires_at))
    );
    assert!(!store.mark_expired(&due.id, now()).await.unwrap());
    // A later call does not move the tombstone clock.
    assert!(
        !store
            .mark_expired(&due.id, now() + Duration::hours(1))
            .await
            .unwrap()
    );
    assert_eq!(state_of(&pool, &due.id).await.1, Some(due.expires_at));

    // A result inside its deadline is never retired early.
    assert!(!store.mark_expired(&live.id, now()).await.unwrap());
    assert_eq!(
        state_of(&pool, &live.id).await,
        ("temporary".to_owned(), None)
    );
}

#[sqlx::test]
async fn expire_due_retires_exactly_the_due_rows(pool: PgPool) {
    let store = ResultsStore::new(pool.clone());
    let past = capture(30, 7, Duration::minutes(40), 100);
    let boundary = capture(31, 8, Duration::minutes(15), 0); // expires_at == now
    let live = capture(32, 7, Duration::minutes(14), 300);
    let kept = capture(33, 7, Duration::hours(2), 400);
    let already = capture(34, 7, Duration::hours(1), 500);
    for row in [&past, &boundary, &live, &kept, &already] {
        insert(&store, row).await;
    }
    keep(&pool, &kept.id).await;
    assert!(store.mark_expired(&already.id, now()).await.unwrap());

    let mut retired = store.expire_due(now()).await.unwrap();
    retired.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(
        retired,
        [
            ExpiredFile {
                id: past.id.clone(),
                file_bytes: 100
            },
            ExpiredFile {
                id: boundary.id.clone(),
                file_bytes: 0
            },
        ]
    );
    for row in [&past, &boundary] {
        assert_eq!(
            state_of(&pool, &row.id).await,
            ("expired".to_owned(), Some(row.expires_at))
        );
    }
    assert_eq!(state_of(&pool, &live.id).await.0, "temporary");
    assert_eq!(state_of(&pool, &kept.id).await.0, "kept");
    assert!(store.expire_due(now()).await.unwrap().is_empty());
}

#[sqlx::test]
async fn delete_tombstones_removes_only_rows_gone_by_the_cutoff(pool: PgPool) {
    let store = ResultsStore::new(pool.clone());
    // Deadlines at now - 2h, now - 1h and now - 30m.
    let oldest = capture(40, 7, Duration::minutes(135), 10);
    let edge = capture(41, 7, Duration::minutes(75), 10);
    let recent = capture(42, 7, Duration::minutes(45), 10);
    let live = capture(43, 7, Duration::minutes(1), 10);
    for row in [&oldest, &edge, &recent, &live] {
        insert(&store, row).await;
    }
    assert_eq!(store.expire_due(now()).await.unwrap().len(), 3);

    let cutoff = now() - Duration::hours(1);
    assert_eq!(edge.expires_at, cutoff);
    assert_eq!(store.delete_tombstones(cutoff).await.unwrap(), 2);
    assert_eq!(store.get(&oldest.id, 7).await.unwrap(), None);
    assert_eq!(store.get(&edge.id, 7).await.unwrap(), None);
    assert!(store.get(&recent.id, 7).await.unwrap().is_some());
    assert!(store.get(&live.id, 7).await.unwrap().is_some());
    assert_eq!(store.delete_tombstones(cutoff).await.unwrap(), 0);
}

/// Usage is what the capture quotas count: live temporary rows only. A
/// kept row is evidence and counts against the evidence budget instead.
#[sqlx::test]
async fn usage_counts_live_temporary_rows_only(pool: PgPool) {
    let store = ResultsStore::new(pool.clone());
    let live = capture(50, 7, Duration::minutes(1), 100);
    let kept = capture(51, 7, Duration::hours(3), 200);
    let due = capture(52, 7, Duration::minutes(16), 400); // past deadline, not swept
    let expired = capture(53, 7, Duration::minutes(30), 800);
    let pruned_old = capture(54, 7, Duration::minutes(10), 1600);
    let other = capture(55, 8, Duration::minutes(1), 3200);
    for row in [&live, &kept, &due, &expired, &pruned_old, &other] {
        insert(&store, row).await;
    }
    keep(&pool, &kept.id).await;
    assert!(store.mark_expired(&expired.id, now()).await.unwrap());
    // Two live temporary rows (100 + 1600 bytes): bring the key to one
    // and 1000 bytes, which prunes the oldest temporary row only.
    assert_eq!(
        ids(&store.prune_oldest(7, 1, 1000, now()).await.unwrap()),
        [pruned_old.id.as_str()]
    );

    assert_eq!(
        store.usage(7, now()).await.unwrap(),
        Usage {
            results: 1,
            bytes: 100
        }
    );
    assert_eq!(
        store.usage(8, now()).await.unwrap(),
        Usage {
            results: 1,
            bytes: 3200
        }
    );
    assert_eq!(store.usage(9, now()).await.unwrap(), Usage::default());
    assert_eq!(
        store.installation_usage(now()).await.unwrap(),
        Usage {
            results: 2,
            bytes: 3300
        }
    );
    // An hour on, no temporary row is live, and the kept one never counts.
    let later = now() + Duration::hours(1);
    assert_eq!(
        store.installation_usage(later).await.unwrap(),
        Usage::default()
    );
    assert_eq!(state_of(&pool, &kept.id).await.0, "kept");
}

#[sqlx::test]
async fn earliest_expiry_names_the_first_live_temporary_deadline(pool: PgPool) {
    let store = ResultsStore::new(pool.clone());
    let young = capture(110, 7, Duration::minutes(1), 100);
    let old = capture(111, 7, Duration::minutes(5), 100);
    let kept = capture(112, 7, Duration::minutes(14), 100);
    let due = capture(113, 7, Duration::minutes(15), 100); // expires exactly now
    let expired = capture(114, 7, Duration::minutes(30), 100);
    let other = capture(115, 8, Duration::minutes(10), 100);
    for row in [&young, &old, &kept, &due, &expired, &other] {
        insert(&store, row).await;
    }
    keep(&pool, &kept.id).await;
    assert!(store.mark_expired(&expired.id, now()).await.unwrap());

    assert_eq!(
        store.earliest_expiry(7, now()).await.unwrap(),
        Some(old.expires_at)
    );
    assert_eq!(
        store.installation_earliest_expiry(now()).await.unwrap(),
        Some(other.expires_at)
    );
    // Key 9 holds nothing; key 7 holds only its kept row, which has no
    // deadline, once both temporary rows are due.
    assert_eq!(store.earliest_expiry(9, now()).await.unwrap(), None);
    let later = now() + Duration::minutes(14);
    assert_eq!(store.earliest_expiry(7, later).await.unwrap(), None);
    assert_eq!(
        store.installation_earliest_expiry(later).await.unwrap(),
        None
    );
}

#[sqlx::test]
async fn live_ids_lists_temporary_and_kept_rows(pool: PgPool) {
    let store = ResultsStore::new(pool.clone());
    let live = capture(60, 7, Duration::minutes(1), 100);
    let due = capture(61, 8, Duration::minutes(20), 200);
    let kept = capture(62, 7, Duration::hours(2), 0);
    let expired = capture(63, 7, Duration::minutes(30), 300);
    for row in [&live, &due, &kept, &expired] {
        insert(&store, row).await;
    }
    keep(&pool, &kept.id).await;
    assert!(store.mark_expired(&expired.id, now()).await.unwrap());

    assert_eq!(
        store.live_ids().await.unwrap(),
        [(live.id, 100), (due.id, 200), (kept.id, 0)]
    );
}

#[sqlx::test]
async fn mark_file_missing_flags_a_live_row_once_and_never_a_gone_one(pool: PgPool) {
    let store = ResultsStore::new(pool.clone());
    let live = capture(64, 7, Duration::minutes(1), 100);
    let kept = capture(65, 7, Duration::hours(2), 100);
    let expired = capture(66, 7, Duration::minutes(30), 100);
    let pruned = capture(67, 9, Duration::minutes(1), 100);
    for row in [&live, &kept, &expired, &pruned] {
        insert(&store, row).await;
    }
    keep(&pool, &kept.id).await;
    assert!(store.mark_expired(&expired.id, now()).await.unwrap());
    assert_eq!(
        ids(&store.prune_oldest(9, 0, u64::MAX, now()).await.unwrap()),
        [pruned.id.as_str()]
    );

    assert!(store.mark_file_missing(&live.id).await.unwrap());
    assert!(
        !store.mark_file_missing(&live.id).await.unwrap(),
        "a second call changes nothing"
    );
    assert!(store.mark_file_missing(&kept.id).await.unwrap());
    // A gone row's file went after its row was marked: nothing was lost.
    assert!(!store.mark_file_missing(&expired.id).await.unwrap());
    assert!(!store.mark_file_missing(&pruned.id).await.unwrap());

    for (row, key, missing) in [
        (&live, 7, true),
        (&kept, 7, true),
        (&expired, 7, false),
        (&pruned, 9, false),
    ] {
        let read = store.get(&row.id, key).await.unwrap().unwrap();
        assert_eq!(read.file_missing, missing, "{:?}", read.state);
    }
}

#[sqlx::test]
async fn prune_oldest_stops_when_the_result_count_fits(pool: PgPool) {
    let store = ResultsStore::new(pool.clone());
    let rows: Vec<NewCapture> = (0..4)
        .map(|n| capture(70 + n, 7, Duration::minutes(i64::from(10 - n)), 10))
        .collect();
    for row in &rows {
        insert(&store, row).await;
    }
    let other = capture(80, 8, Duration::minutes(12), 10);
    insert(&store, &other).await;

    let pruned = store.prune_oldest(7, 2, u64::MAX, now()).await.unwrap();
    assert_eq!(ids(&pruned), [rows[0].id.as_str(), rows[1].id.as_str()]);
    for row in &rows[..2] {
        assert_eq!(
            state_of(&pool, &row.id).await,
            ("pruned".to_owned(), Some(now()))
        );
    }
    for row in &rows[2..] {
        assert_eq!(state_of(&pool, &row.id).await.0, "temporary");
    }
    assert_eq!(state_of(&pool, &other.id).await.0, "temporary");
    assert_eq!(store.usage(7, now()).await.unwrap().results, 2);
    // Already inside the limits: nothing more goes.
    assert!(
        store
            .prune_oldest(7, 2, u64::MAX, now())
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test]
async fn prune_oldest_stops_when_the_bytes_fit(pool: PgPool) {
    let store = ResultsStore::new(pool.clone());
    let first = capture(90, 7, Duration::minutes(9), 100);
    let second = capture(91, 7, Duration::minutes(8), 200);
    let third = capture(92, 7, Duration::minutes(7), 300);
    for row in [&first, &second, &third] {
        insert(&store, row).await;
    }

    let pruned = store.prune_oldest(7, u64::MAX, 350, now()).await.unwrap();
    assert_eq!(
        pruned,
        [
            ExpiredFile {
                id: first.id.clone(),
                file_bytes: 100
            },
            ExpiredFile {
                id: second.id.clone(),
                file_bytes: 200
            },
        ]
    );
    assert_eq!(
        store.usage(7, now()).await.unwrap(),
        Usage {
            results: 1,
            bytes: 300
        }
    );
}

/// Kept rows and rows past their deadline are neither counted nor pruned.
#[sqlx::test]
async fn prune_oldest_never_counts_or_prunes_kept_or_due_rows(pool: PgPool) {
    let store = ResultsStore::new(pool.clone());
    let kept = capture(100, 7, Duration::hours(3), 1000);
    let due = capture(101, 7, Duration::minutes(20), 10);
    let live = capture(102, 7, Duration::minutes(1), 10);
    for row in [&kept, &due, &live] {
        insert(&store, row).await;
    }
    keep(&pool, &kept.id).await;

    // One live temporary row of 10 bytes fits these limits: the kept
    // row's 1000 bytes are not counted.
    assert!(
        store
            .prune_oldest(7, 1, 500, now())
            .await
            .unwrap()
            .is_empty()
    );
    let pruned = store.prune_oldest(7, 0, 0, now()).await.unwrap();
    assert_eq!(ids(&pruned), [live.id.as_str()]);
    assert_eq!(state_of(&pool, &kept.id).await.0, "kept");
    assert_eq!(state_of(&pool, &due.id).await.0, "temporary");
    assert_eq!(store.usage(7, now()).await.unwrap(), Usage::default());
}

/// Boot walks the keys holding live temporary results for the per-key
/// quota pass: a kept, past-deadline or expired row does not count.
#[sqlx::test]
async fn live_keys_names_each_key_holding_a_live_result_once(pool: PgPool) {
    let store = ResultsStore::new(pool.clone());
    let rows = [
        capture(110, 9, Duration::minutes(1), 10),
        capture(111, 7, Duration::minutes(2), 10),
        capture(112, 9, Duration::minutes(3), 10),
        capture(113, 11, Duration::hours(2), 0),
        capture(114, 12, Duration::minutes(20), 10),
        capture(115, 13, Duration::minutes(30), 10),
    ];
    for row in &rows {
        insert(&store, row).await;
    }
    keep(&pool, &rows[3].id).await;
    assert!(store.mark_expired(&rows[5].id, now()).await.unwrap());

    assert_eq!(store.live_keys(now()).await.unwrap(), [7, 9]);
}

/// The installation prune takes the oldest temporary rows across keys
/// until both installation limits fit, and neither counts nor touches
/// kept rows.
#[sqlx::test]
async fn prune_oldest_installation_prunes_across_keys_until_both_limits_fit(pool: PgPool) {
    let store = ResultsStore::new(pool.clone());
    let kept = capture(120, 9, Duration::hours(3), 1000);
    let oldest = capture(121, 7, Duration::minutes(9), 100);
    let older = capture(122, 8, Duration::minutes(8), 200);
    let newer = capture(123, 7, Duration::minutes(7), 300);
    for row in [&kept, &oldest, &older, &newer] {
        insert(&store, row).await;
    }
    keep(&pool, &kept.id).await;

    // Three live temporary rows and 600 bytes; the kept row's 1000 bytes
    // do not count. Two rows fit, 600 bytes do not.
    let pruned = store
        .prune_oldest_installation(2, 300, now())
        .await
        .unwrap();
    assert_eq!(ids(&pruned), [oldest.id.as_str(), older.id.as_str()]);
    for row in [&oldest, &older] {
        assert_eq!(
            state_of(&pool, &row.id).await,
            ("pruned".to_owned(), Some(now()))
        );
    }
    assert_eq!(state_of(&pool, &newer.id).await.0, "temporary");
    assert_eq!(state_of(&pool, &kept.id).await.0, "kept");
    assert_eq!(
        store.installation_usage(now()).await.unwrap(),
        Usage {
            results: 1,
            bytes: 300
        }
    );
    assert!(
        store
            .prune_oldest_installation(2, 300, now())
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test]
async fn a_digest_without_a_file_or_a_file_without_a_digest_is_refused(pool: PgPool) {
    let store = ResultsStore::new(pool.clone());
    let no_digest = NewCapture {
        digest: None,
        ..capture(110, 7, Duration::zero(), 10)
    };
    assert_eq!(
        violated(store.insert(&no_digest).await.unwrap_err()),
        "captured_results_digest_iff_file"
    );
    let no_file = NewCapture {
        digest: Some([1; 32]),
        ..capture(111, 7, Duration::zero(), 0)
    };
    assert_eq!(
        violated(store.insert(&no_file).await.unwrap_err()),
        "captured_results_digest_iff_file"
    );
    assert_eq!(
        store.installation_usage(now()).await.unwrap(),
        Usage::default()
    );
}

#[sqlx::test]
async fn an_id_that_is_not_32_lowercase_hex_digits_is_refused(pool: PgPool) {
    let store = ResultsStore::new(pool.clone());
    for bad in [
        id(120).to_uppercase().replace('0', "A"),
        "0123456789ABCDEF0123456789abcdef".to_owned(),
        "0123456789abcdef".to_owned(),
        format!("{}0", id(121)),
        "../0123456789abcdef0123456789abc".to_owned(),
    ] {
        let row = NewCapture {
            id: bad.clone(),
            ..capture(122, 7, Duration::zero(), 10)
        };
        assert_eq!(
            violated(store.insert(&row).await.unwrap_err()),
            "captured_results_id_hex",
            "{bad}"
        );
    }
}

/// A row whose metadata does not decode is a store error naming the
/// column, never a panic.
#[sqlx::test]
async fn corrupt_metadata_is_an_error_not_a_panic(pool: PgPool) {
    let store = ResultsStore::new(pool.clone());
    let row = capture(130, 7, Duration::zero(), 10);
    insert(&store, &row).await;
    sqlx::query(r#"UPDATE captured_results SET column_facts = '[{"nulls": -1}]' WHERE id = $1"#)
        .bind(&row.id)
        .execute(&pool)
        .await
        .unwrap();

    for err in [
        store.get(&row.id, 7).await.unwrap_err(),
        store.list(7, now()).await.unwrap_err(),
    ] {
        match err {
            StoreError::Unavailable(sqlx::Error::ColumnDecode { index, .. }) => {
                assert_eq!(index, "column_facts");
            }
            other => panic!("expected a column decode error, got {other:?}"),
        }
    }
}

/// The facts of a sightings read over `literal`, enough to round-trip.
fn sightings(literal: &str) -> Correlation {
    Correlation::Sightings(SightingsFacts {
        literal: literal.to_owned(),
        matches_total: 20_000,
        captured: 5000,
        attribution_fields: vec!["client_ip".into()],
        groups: vec![SightingGroup {
            env: Some("prod".into()),
            service: None,
            count: 20_000,
            first_seen: "2026-10-23T09:00:00.000000Z".into(),
            last_seen: "2026-10-23T09:59:59.999999Z".into(),
        }],
        fields: vec![FieldSightings {
            field: "client_ip".into(),
            rows: 4999,
        }],
        fields_omitted: 0,
        raw_only_rows: 1,
        denominators: SightingsDenominators {
            field_counts_over: 5000,
            service_counts_over: 20_000,
        },
        follow_ups: vec!["\"10.0.4.17\" | timechart span=1h count() by service".into()],
    })
}

/// A correlation read's kind and facts round-trip through insert, get and
/// list to the wire. The facts are TEXT, so a literal holding U+0000
/// commits like any other; a search reads back as a search with no facts.
#[sqlx::test]
async fn a_correlation_capture_round_trips_its_kind_and_facts(pool: PgPool) {
    let store = ResultsStore::new(pool.clone());
    let facts = sightings("10.0.4.17\0");
    let row = NewCapture {
        kind: CaptureKind::Sightings,
        correlation: Some(FactsDocument::encode(facts.clone()).unwrap()),
        ..capture(140, 7, Duration::zero(), 10)
    };
    insert(&store, &row).await;
    let search = capture(141, 7, Duration::seconds(1), 10);
    insert(&store, &search).await;

    let got = store.get(&row.id, 7).await.unwrap().expect("the row");
    assert_eq!(got.kind, CaptureKind::Sightings);
    assert_eq!(got.correlation, Some(facts.clone()));
    let wire = got.to_wire();
    assert_eq!(wire.kind, CaptureKind::Sightings);
    assert_eq!(wire.correlation, Some(facts.clone()));

    let listed = store.list(7, now()).await.unwrap();
    let wires: Vec<_> = listed.iter().map(CaptureRow::to_wire).collect();
    assert_eq!(wires.len(), 2);
    assert_eq!(
        (wires[0].kind, wires[0].correlation.as_ref()),
        (CaptureKind::Sightings, Some(&facts))
    );
    assert_eq!(
        (wires[1].kind, wires[1].correlation.as_ref()),
        (CaptureKind::Search, None)
    );
    let spelled = serde_json::to_value(&wires[1]).unwrap();
    assert!(spelled.get("kind").is_none() && spelled.get("correlation").is_none());
}

/// A capture's facts document counts in its bytes: the row records its
/// length as `metadata_bytes`, the key's and the installation's usage add
/// it to `file_bytes`, and the prune counts both. The file's size stays
/// what it is.
#[sqlx::test]
async fn usage_and_the_prune_count_the_facts_document(pool: PgPool) {
    let store = ResultsStore::new(pool.clone());
    let document = FactsDocument::encode(sightings("10.0.4.17")).unwrap();
    let facts_bytes = document.bytes();
    let row = NewCapture {
        kind: CaptureKind::Sightings,
        correlation: Some(document),
        ..capture(170, 7, Duration::seconds(2), 100)
    };
    insert(&store, &row).await;
    insert(&store, &capture(171, 7, Duration::seconds(1), 100)).await;

    let got = store.get(&row.id, 7).await.unwrap().expect("the row");
    assert_eq!((got.file_bytes, got.metadata_bytes), (100, facts_bytes));
    assert_eq!(got.to_wire().file_bytes, 100);
    let stored: i64 = sqlx::query_scalar(
        "SELECT octet_length(correlation)::bigint FROM captured_results WHERE id = $1",
    )
    .bind(&row.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(u64::try_from(stored).unwrap(), facts_bytes);
    let search = store.get(&id(171), 7).await.unwrap().unwrap();
    assert_eq!(search.metadata_bytes, 0);

    let both = Usage {
        results: 2,
        bytes: 200 + facts_bytes,
    };
    assert_eq!(store.usage(7, now()).await.unwrap(), both);
    assert_eq!(store.installation_usage(now()).await.unwrap(), both);

    // 200 file bytes fit; the facts do not, so the oldest goes.
    assert_eq!(
        ids(&store.prune_oldest(7, u64::MAX, 200, now()).await.unwrap()),
        [id(170).as_str()]
    );
    assert_eq!(
        store.usage(7, now()).await.unwrap(),
        Usage {
            results: 1,
            bytes: 100
        }
    );
}

/// Every kind the wire names is one the table takes, and the table refuses
/// any other under its named constraint.
#[sqlx::test]
async fn every_capture_kind_is_stored_and_no_other(pool: PgPool) {
    let store = ResultsStore::new(pool.clone());
    for (n, kind) in [
        CaptureKind::Search,
        CaptureKind::Context,
        CaptureKind::Sightings,
        CaptureKind::Compare,
    ]
    .into_iter()
    .enumerate()
    {
        let row = NewCapture {
            kind,
            ..capture(150 + u32::try_from(n).unwrap(), 7, Duration::zero(), 10)
        };
        insert(&store, &row).await;
        assert_eq!(store.get(&row.id, 7).await.unwrap().unwrap().kind, kind);
    }
    let refused = sqlx::query("UPDATE captured_results SET kind = 'anomaly' WHERE id = $1")
        .bind(id(150))
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(
        refused
            .as_database_error()
            .and_then(|e| e.constraint())
            .map(str::to_owned)
            .as_deref(),
        Some("captured_results_kind_check")
    );
}

/// Stored facts that no longer decode are a store error naming the column,
/// never a panic.
#[sqlx::test]
async fn corrupt_correlation_facts_are_an_error_not_a_panic(pool: PgPool) {
    let store = ResultsStore::new(pool.clone());
    let row = NewCapture {
        kind: CaptureKind::Sightings,
        correlation: Some(FactsDocument::encode(sightings("10.0.4.17")).unwrap()),
        ..capture(160, 7, Duration::zero(), 10)
    };
    insert(&store, &row).await;
    sqlx::query(r#"UPDATE captured_results SET correlation = '{"kind": "anomaly"}' WHERE id = $1"#)
        .bind(&row.id)
        .execute(&pool)
        .await
        .unwrap();

    for err in [
        store.get(&row.id, 7).await.unwrap_err(),
        store.list(7, now()).await.unwrap_err(),
    ] {
        match err {
            StoreError::Unavailable(sqlx::Error::ColumnDecode { index, .. }) => {
                assert_eq!(index, "correlation");
            }
            other => panic!("expected a column decode error, got {other:?}"),
        }
    }
}
