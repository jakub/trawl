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
use trawl_api::results::{
    ColumnFacts, ResultColumn, ResultShape, ResultSource, ResultState, StoredAs, ValueCount,
};
use trawl_api::value::{ColumnType, Value};
use trawl_server::store::{
    CaptureInsert, ExpiredFile, NewCapture, ResultsStore, StoreError, Usage,
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
    }
}

async fn insert(store: &ResultsStore, row: &NewCapture) {
    assert_eq!(store.insert(row).await.unwrap(), CaptureInsert::Inserted);
}

/// Keep a row the way an investigation will (#308): no API does it yet.
async fn keep(pool: &PgPool, id: &str) {
    sqlx::query(
        "UPDATE captured_results SET state = 'kept', expires_at = NULL, investigation_id = 1
         WHERE id = $1",
    )
    .bind(id)
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
    assert_eq!(got.investigation_id, Some(1));
    let wire = got.to_wire();
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

#[sqlx::test]
async fn list_is_newest_first_live_only_and_includes_kept(pool: PgPool) {
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
    assert_eq!(listed, [new.id, old.id, kept.id]);
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

#[sqlx::test]
async fn usage_counts_live_and_kept_rows_only(pool: PgPool) {
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
    // Three live rows (100 + 200 + 1600 bytes): bring the key to two and
    // 300 bytes, which prunes the oldest temporary row only.
    assert_eq!(
        ids(&store.prune_oldest(7, 2, 300, now()).await.unwrap()),
        [pruned_old.id.as_str()]
    );

    assert_eq!(
        store.usage(7, now()).await.unwrap(),
        Usage {
            results: 2,
            bytes: 300
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
            results: 3,
            bytes: 3500
        }
    );
    // An hour on, only the kept row is live.
    let later = now() + Duration::hours(1);
    assert_eq!(
        store.installation_usage(later).await.unwrap(),
        Usage {
            results: 1,
            bytes: 200
        }
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

/// Kept rows count toward the limits but are never pruned, and rows past
/// their deadline are neither counted nor pruned.
#[sqlx::test]
async fn prune_oldest_never_prunes_kept_or_due_rows(pool: PgPool) {
    let store = ResultsStore::new(pool.clone());
    let kept = capture(100, 7, Duration::hours(3), 1000);
    let due = capture(101, 7, Duration::minutes(20), 10);
    let live = capture(102, 7, Duration::minutes(1), 10);
    for row in [&kept, &due, &live] {
        insert(&store, row).await;
    }
    keep(&pool, &kept.id).await;

    let pruned = store.prune_oldest(7, 1, 500, now()).await.unwrap();
    assert_eq!(ids(&pruned), [live.id.as_str()]);
    assert_eq!(state_of(&pool, &kept.id).await.0, "kept");
    assert_eq!(state_of(&pool, &due.id).await.0, "temporary");
    assert_eq!(
        store.usage(7, now()).await.unwrap(),
        Usage {
            results: 1,
            bytes: 1000
        }
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
