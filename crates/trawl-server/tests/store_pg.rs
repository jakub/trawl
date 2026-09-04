// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres store tests for trawl-server's app-state stores.
//!
//! Covers per-store behaviour, the concurrency guarantees postgres has to
//! make on its own (nothing serialises these calls in-process), and the
//! boot path: advisory lock, migration, idempotency, failure modes.
//!
//! Plain `#[sqlx::test]` auto-applies `crates/trawl-server/migrations/` to
//! each per-test database. The boot tests take no harness pool at all:
//! they mint their own database and drive it through `StorageState`, so
//! `#[sqlx::test]` would only mint a second database nobody opens and
//! charge its 5-connection pool to the admission budget for the duration
//! (`common::BOOT_CONNECTION_CEILING`). They keep the real boot path:
//! `from_pool` is where the advisory lock and the migration live, and the
//! pool it is handed is `common::app_pool`, sized like the fixture's
//! rather than trawld's production 8.

mod common;

use sqlx::PgPool;
use trawl_server::store::{
    FinishOutcome, FlipOutcome, HistoryStore, RunClaim, RunStatus, SavedQueryStore, ScheduleStore,
    StorageState, StoreError,
};

fn history(pool: &PgPool) -> HistoryStore {
    HistoryStore::new(pool.clone())
}

fn saved(pool: &PgPool) -> SavedQueryStore {
    SavedQueryStore::new(pool.clone())
}

fn schedules(pool: &PgPool) -> ScheduleStore {
    ScheduleStore::new(pool.clone())
}

// ---------------------------------------------------------------------------
// history
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn history_record_and_retrieve(pool: PgPool) {
    let store = history(&pool);
    let key_id = 42;

    let id = store
        .record_query(
            key_id,
            "service=nginx | stats count()",
            123,
            5,
            RunStatus::Success,
        )
        .await
        .unwrap();
    assert!(id > 0);

    let page = store.get_user_history(key_id, 10, 0).await.unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.entries.len(), 1);

    let entry = &page.entries[0];
    assert_eq!(entry.key_id, key_id);
    assert_eq!(entry.query, "service=nginx | stats count()");
    assert_eq!(entry.duration_ms, 123);
    assert_eq!(entry.row_count, 5);
    assert_eq!(entry.status, RunStatus::Success);
}

#[sqlx::test]
async fn history_pagination(pool: PgPool) {
    let store = history(&pool);
    let key_id = 1;

    for i in 0..5 {
        store
            .record_query(key_id, &format!("query {i}"), 100, 10, RunStatus::Success)
            .await
            .unwrap();
    }

    let page1 = store.get_user_history(key_id, 2, 0).await.unwrap();
    assert_eq!(page1.total, 5);
    assert_eq!(page1.entries.len(), 2);
    // Most recent first (descending order).
    assert_eq!(page1.entries[0].query, "query 4");
    assert_eq!(page1.entries[1].query, "query 3");

    let page2 = store.get_user_history(key_id, 2, 2).await.unwrap();
    assert_eq!(page2.entries[0].query, "query 2");
    assert_eq!(page2.entries[1].query, "query 1");

    let page3 = store.get_user_history(key_id, 2, 4).await.unwrap();
    assert_eq!(page3.entries.len(), 1);
    assert_eq!(page3.entries[0].query, "query 0");
}

/// Same-timestamp rows page deterministically: `ORDER BY executed_at DESC,
/// id DESC` tie-breaks on id, so pagination never duplicates or skips.
#[sqlx::test]
async fn history_pagination_tie_breaks_on_id(pool: PgPool) {
    for i in 0..4 {
        sqlx::query(
            "INSERT INTO query_history (key_id, query, executed_at, duration_ms, row_count, status)
             VALUES (1, $1, '2026-07-01T00:00:00Z', 1, 1, 'success')",
        )
        .bind(format!("query {i}"))
        .execute(&pool)
        .await
        .unwrap();
    }

    let store = history(&pool);
    let page1 = store.get_user_history(1, 2, 0).await.unwrap();
    let page2 = store.get_user_history(1, 2, 2).await.unwrap();
    let seen: Vec<&str> = page1
        .entries
        .iter()
        .chain(page2.entries.iter())
        .map(|e| e.query.as_str())
        .collect();
    assert_eq!(
        seen,
        vec!["query 3", "query 2", "query 1", "query 0"],
        "tie-broken pages must be a deterministic, gap-free sequence"
    );
}

#[sqlx::test]
async fn history_user_isolation(pool: PgPool) {
    let store = history(&pool);

    store
        .record_query(1, "user 1 query", 50, 10, RunStatus::Success)
        .await
        .unwrap();
    store
        .record_query(2, "user 2 query", 75, 20, RunStatus::Success)
        .await
        .unwrap();
    store
        .record_query(1, "user 1 query 2", 100, 15, RunStatus::Success)
        .await
        .unwrap();

    let user1 = store.get_user_history(1, 10, 0).await.unwrap();
    assert_eq!(user1.total, 2);
    assert_eq!(user1.entries[0].query, "user 1 query 2");

    let user2 = store.get_user_history(2, 10, 0).await.unwrap();
    assert_eq!(user2.total, 1);
    assert_eq!(user2.entries[0].query, "user 2 query");
}

#[sqlx::test]
async fn history_empty(pool: PgPool) {
    let store = history(&pool);
    let page = store.get_user_history(999, 10, 0).await.unwrap();
    assert_eq!(page.total, 0);
    assert_eq!(page.entries.len(), 0);
}

/// The `RunStatus` enum keeps the typed write path inside the status domain;
/// the DB CHECK is the backstop against a raw write bypassing it. Prove the
/// constraint (23514) still fires when an out-of-domain value is inserted
/// directly.
#[sqlx::test]
async fn history_status_check_is_backstop(pool: PgPool) {
    let err = sqlx::query(
        "INSERT INTO query_history (key_id, query, executed_at, duration_ms, row_count, status)
         VALUES (1, 'q', now(), 1, 1, 'bogus')",
    )
    .execute(&pool)
    .await
    .expect_err("CHECK must reject an out-of-domain status");
    let db = err.as_database_error().expect("database error");
    assert_eq!(db.code().as_deref(), Some("23514"), "got: {err:?}");
}

// ---------------------------------------------------------------------------
// saved queries
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn saved_create_and_list(pool: PgPool) {
    let store = saved(&pool);
    let key_id = 1;

    let created = store
        .create(key_id, "nginx_errors", "service=nginx _severity=error")
        .await
        .unwrap();
    assert_eq!(created.name, "nginx_errors");
    assert_eq!(created.query, "service=nginx _severity=error");

    let list = store.list(key_id).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "nginx_errors");
}

#[sqlx::test]
async fn saved_duplicate_name_returns_error(pool: PgPool) {
    let store = saved(&pool);
    store.create(1, "test", "query 1").await.unwrap();
    let result = store.create(1, "test", "query 2").await;
    assert!(matches!(result, Err(StoreError::DuplicateName { .. })));
}

#[sqlx::test]
async fn saved_update(pool: PgPool) {
    let store = saved(&pool);
    let created = store.create(1, "test", "original query").await.unwrap();

    let updated = store
        .update(created.id, 1, "updated query", None)
        .await
        .unwrap();
    assert_eq!(updated.query, "updated query");
    assert_eq!(updated.name, "test");
    assert_ne!(updated.updated_at, updated.created_at);
}

#[sqlx::test]
async fn saved_update_with_rename(pool: PgPool) {
    let store = saved(&pool);
    let created = store.create(1, "old-name", "query").await.unwrap();

    let updated = store
        .update(created.id, 1, "query", Some("new-name"))
        .await
        .unwrap();
    assert_eq!(updated.name, "new-name");
    assert_eq!(updated.query, "query");
}

#[sqlx::test]
async fn saved_rename_to_taken_name_conflicts(pool: PgPool) {
    let store = saved(&pool);
    store.create(1, "taken", "q").await.unwrap();
    let other = store.create(1, "other", "q").await.unwrap();
    let result = store.update(other.id, 1, "q", Some("taken")).await;
    assert!(matches!(result, Err(StoreError::DuplicateName { .. })));
}

#[sqlx::test]
async fn saved_update_nonexistent_returns_error(pool: PgPool) {
    let store = saved(&pool);
    let result = store.update(999, 1, "query", None).await;
    assert!(matches!(result, Err(StoreError::NotFound { .. })));
}

#[sqlx::test]
async fn saved_update_other_users_query_returns_error(pool: PgPool) {
    let store = saved(&pool);
    let created = store.create(1, "test", "query").await.unwrap();
    let result = store.update(created.id, 2, "updated", None).await;
    assert!(matches!(result, Err(StoreError::NotFound { .. })));
}

#[sqlx::test]
async fn saved_delete(pool: PgPool) {
    let store = saved(&pool);
    let created = store.create(1, "test", "query").await.unwrap();
    let paths = store.delete(created.id, 1).await.unwrap();
    assert!(paths.is_empty());
    assert_eq!(store.list(1).await.unwrap().len(), 0);
}

#[sqlx::test]
async fn saved_delete_nonexistent_returns_error(pool: PgPool) {
    let store = saved(&pool);
    let result = store.delete(999, 1).await;
    assert!(matches!(result, Err(StoreError::NotFound { .. })));
}

#[sqlx::test]
async fn saved_delete_other_users_query_returns_error(pool: PgPool) {
    let store = saved(&pool);
    let created = store.create(1, "test", "query").await.unwrap();
    let result = store.delete(created.id, 2).await;
    assert!(matches!(result, Err(StoreError::NotFound { .. })));
}

#[sqlx::test]
async fn saved_user_isolation_and_name_sort(pool: PgPool) {
    let store = saved(&pool);

    store.create(1, "z_last", "query 1").await.unwrap();
    store.create(2, "user2_query", "query 2").await.unwrap();
    store.create(1, "a_first", "query 3").await.unwrap();

    let user1 = store.list(1).await.unwrap();
    assert_eq!(user1.len(), 2);
    assert_eq!(user1[0].name, "a_first");
    assert_eq!(user1[1].name, "z_last");

    let user2 = store.list(2).await.unwrap();
    assert_eq!(user2.len(), 1);
    assert_eq!(user2[0].name, "user2_query");
}

#[sqlx::test]
async fn saved_name_validation(pool: PgPool) {
    let store = saved(&pool);
    for bad in ["has spaces", "foo/bar", "foo.bar", ""] {
        assert!(
            matches!(
                store.create(1, bad, "q").await,
                Err(StoreError::InvalidName { .. })
            ),
            "{bad:?} must be rejected"
        );
    }
    for good in ["daily_ip_rollup", "hourly-error-count", "Auth2", "a"] {
        store.create(1, good, "q").await.unwrap();
    }
}

#[sqlx::test]
async fn saved_get_by_name(pool: PgPool) {
    let store = saved(&pool);
    assert!(store.get_by_name(1, "missing").await.unwrap().is_none());

    store
        .create(1, "my_query", "_severity=error")
        .await
        .unwrap();

    let found = store.get_by_name(1, "my_query").await.unwrap().unwrap();
    assert_eq!(found.name, "my_query");
    assert_eq!(found.query, "_severity=error");

    // Other user can't see it.
    assert!(store.get_by_name(2, "my_query").await.unwrap().is_none());
}

/// The bulk-join list carries schedule + latest run + run count per item in
/// a single statement, so the query count does not grow with the list.
#[sqlx::test]
async fn saved_list_with_details_bulk_join(pool: PgPool) {
    let saved_store = saved(&pool);
    let schedule_store = schedules(&pool);

    let a = saved_store.create(1, "alpha", "q1").await.unwrap();
    let b = saved_store.create(1, "beta", "q2").await.unwrap();
    saved_store.create(1, "gamma", "q3").await.unwrap();

    let sched_a = schedule_store
        .create_schedule(a.id, 1, 300, Some(10))
        .await
        .unwrap();
    schedule_store
        .create_schedule(b.id, 1, 600, None)
        .await
        .unwrap();

    // Two finished runs on alpha.
    for i in 0..2 {
        let rid = seed_run(&schedule_store, sched_a.id, a.id, "q1").await;
        assert_eq!(
            schedule_store
                .finish_run(rid, RunStatus::Success, 100 + i, Some(5), None, None, None)
                .await
                .unwrap(),
            FinishOutcome::Persisted
        );
    }

    let details = saved_store.list_with_details(1).await.unwrap();
    assert_eq!(details.len(), 3);
    assert_eq!(details[0].saved.name, "alpha");

    let alpha = details[0].schedule.as_ref().expect("alpha has schedule");
    assert_eq!(alpha.schedule.interval_secs, 300);
    assert_eq!(alpha.schedule.max_runs, Some(10));
    assert_eq!(alpha.total_runs, 2);
    let latest = alpha.latest_run.as_ref().expect("latest run present");
    assert_eq!(latest.duration_ms, Some(101), "latest run is the newest");

    let beta = details[1].schedule.as_ref().expect("beta has schedule");
    assert_eq!(beta.total_runs, 0);
    assert!(beta.latest_run.is_none());

    assert!(details[2].schedule.is_none(), "gamma has no schedule");
}

// ---------------------------------------------------------------------------
// schedules + runs
// ---------------------------------------------------------------------------

async fn seed_saved(pool: &PgPool, key_id: i64, name: &str) -> i64 {
    saved(pool)
        .create(key_id, name, "_severity=error")
        .await
        .unwrap()
        .id
}

/// Claim a `running` report run and return its id, asserting the claim
/// started (no `max_runs` cap). Seeds runs for the schedule/run lifecycle
/// tests without the concurrency guards those tests set up explicitly.
async fn seed_run(
    store: &ScheduleStore,
    schedule_id: i64,
    saved_query_id: i64,
    query: &str,
) -> i64 {
    match store
        .claim_run(schedule_id, saved_query_id, query, None)
        .await
        .unwrap()
    {
        RunClaim::Started(id) => id,
        other => panic!("expected a started run, got {other:?}"),
    }
}

#[sqlx::test]
async fn schedule_create_and_get(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "test-query").await;

    let schedule = store.create_schedule(sq_id, 1, 300, None).await.unwrap();
    assert_eq!(schedule.saved_query_id, sq_id);
    assert_eq!(schedule.interval_secs, 300);
    assert!(schedule.enabled);
    assert!(schedule.max_runs.is_none());

    let fetched = store
        .get_schedule_for_saved_query(sq_id, 1)
        .await
        .unwrap()
        .expect("schedule should exist");
    assert_eq!(fetched.id, schedule.id);
}

#[sqlx::test]
async fn schedule_duplicate_returns_error(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "test").await;

    store.create_schedule(sq_id, 1, 300, None).await.unwrap();
    let result = store.create_schedule(sq_id, 1, 600, None).await;
    assert!(matches!(result, Err(StoreError::ScheduleExists { .. })));
}

#[sqlx::test]
async fn schedule_for_missing_saved_query_is_not_found(pool: PgPool) {
    // 23503 on the named FK maps to NotFound, not a raw pg error.
    let store = schedules(&pool);
    let result = store.create_schedule(12345, 1, 300, None).await;
    assert!(matches!(result, Err(StoreError::NotFound { .. })));
}

#[sqlx::test]
async fn schedule_rejects_sub_minute_interval(pool: PgPool) {
    // The 60s minimum is enforced on both the create and update paths, not
    // only where `parse_interval` reads the request string.
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "test").await;

    assert!(matches!(
        store.create_schedule(sq_id, 1, 30, None).await,
        Err(StoreError::IntervalTooShort { secs: 30 })
    ));

    let created = store.create_schedule(sq_id, 1, 300, None).await.unwrap();
    assert!(matches!(
        store.update_schedule(created.id, 1, 45, None, true).await,
        Err(StoreError::IntervalTooShort { secs: 45 })
    ));
}

#[sqlx::test]
async fn schedule_update(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "test").await;

    let created = store.create_schedule(sq_id, 1, 300, None).await.unwrap();
    let updated = store
        .update_schedule(created.id, 1, 600, Some(10), false)
        .await
        .unwrap();
    assert_eq!(updated.interval_secs, 600);
    assert_eq!(updated.max_runs, Some(10));
    assert!(!updated.enabled);
}

#[sqlx::test]
async fn schedule_update_other_user_not_found(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "test").await;
    let created = store.create_schedule(sq_id, 1, 300, None).await.unwrap();
    let result = store.update_schedule(created.id, 2, 600, None, true).await;
    assert!(matches!(result, Err(StoreError::NotFound { .. })));
}

#[sqlx::test]
async fn schedule_delete(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "test").await;
    store.create_schedule(sq_id, 1, 300, None).await.unwrap();

    let paths = store.delete_schedule(sq_id, 1).await.unwrap();
    assert!(paths.is_empty());
    assert!(
        store
            .get_schedule_for_saved_query(sq_id, 1)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test]
async fn schedule_delete_other_user_not_found(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "test").await;
    store.create_schedule(sq_id, 1, 300, None).await.unwrap();
    let result = store.delete_schedule(sq_id, 2).await;
    assert!(matches!(result, Err(StoreError::NotFound { .. })));
}

#[sqlx::test]
async fn schedule_user_isolation(pool: PgPool) {
    let store = schedules(&pool);
    let sq1 = seed_saved(&pool, 1, "user1-query").await;
    let sq2 = seed_saved(&pool, 2, "user2-query").await;

    store.create_schedule(sq1, 1, 300, None).await.unwrap();
    store.create_schedule(sq2, 2, 600, None).await.unwrap();

    assert!(
        store
            .get_schedule_for_saved_query(sq2, 1)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get_schedule_for_saved_query(sq1, 2)
            .await
            .unwrap()
            .is_none()
    );
}

/// Full cascade chain: deleting the saved query wipes its schedule and its
/// runs, and the delete transaction reports the parquet paths to unlink.
#[sqlx::test]
async fn cascade_on_saved_query_delete(pool: PgPool) {
    let saved_store = saved(&pool);
    let store = schedules(&pool);
    let sq = saved_store
        .create(1, "test", "_severity=error")
        .await
        .unwrap();
    let schedule = store.create_schedule(sq.id, 1, 300, None).await.unwrap();

    let run_id = seed_run(&store, schedule.id, sq.id, "_severity=error").await;
    assert_eq!(
        store
            .finish_run(
                run_id,
                RunStatus::Success,
                100,
                Some(5),
                None,
                None,
                Some("scheduled/test/run_1.parquet"),
            )
            .await
            .unwrap(),
        FinishOutcome::Persisted
    );

    let paths = saved_store.delete(sq.id, 1).await.unwrap();
    assert_eq!(paths, vec!["scheduled/test/run_1.parquet".to_string()]);

    assert!(
        store
            .get_schedule_for_saved_query(sq.id, 1)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(store.count_runs(schedule.id).await.unwrap(), 0);
}

#[sqlx::test]
async fn schedule_delete_collects_run_paths(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "test").await;
    let schedule = store.create_schedule(sq_id, 1, 300, None).await.unwrap();

    let run_id = seed_run(&store, schedule.id, sq_id, "q").await;
    store
        .finish_run(
            run_id,
            RunStatus::Success,
            10,
            Some(1),
            None,
            None,
            Some("scheduled/test/run_9.parquet"),
        )
        .await
        .unwrap();

    let paths = store.delete_schedule(sq_id, 1).await.unwrap();
    assert_eq!(paths, vec!["scheduled/test/run_9.parquet".to_string()]);
}

// ---------------------------------------------------------------------------
// run lifecycle
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn start_and_finish_run(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "test").await;
    let schedule = store.create_schedule(sq_id, 1, 300, None).await.unwrap();

    let run_id = seed_run(&store, schedule.id, sq_id, "_severity=error").await;

    let run = store.get_run(run_id, 1).await.unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Running);

    assert_eq!(
        store
            .finish_run(
                run_id,
                RunStatus::Success,
                150,
                Some(42),
                None,
                Some(b"compressed-data"),
                None,
            )
            .await
            .unwrap(),
        FinishOutcome::Persisted
    );

    let finished = store.get_run(run_id, 1).await.unwrap().unwrap();
    assert_eq!(finished.status, RunStatus::Success);
    assert_eq!(finished.duration_ms, Some(150));
    assert_eq!(finished.row_count, Some(42));

    let blob = store.get_run_result(run_id, 1).await.unwrap().unwrap();
    assert_eq!(blob, b"compressed-data");
}

#[sqlx::test]
async fn sequential_run_prevention(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "test").await;
    let schedule = store.create_schedule(sq_id, 1, 300, None).await.unwrap();

    let first = store
        .claim_run(schedule.id, sq_id, "q", None)
        .await
        .unwrap();
    let RunClaim::Started(first_id) = first else {
        panic!("first claim must start a run, got {first:?}");
    };

    // Second claim is refused (already running) via the named 23505.
    let second = store
        .claim_run(schedule.id, sq_id, "q", None)
        .await
        .unwrap();
    assert!(matches!(second, RunClaim::AlreadyRunning), "got {second:?}");

    // After finishing, a new run can start.
    store
        .finish_run(first_id, RunStatus::Success, 100, None, None, None, None)
        .await
        .unwrap();
    let third = store
        .claim_run(schedule.id, sq_id, "q", None)
        .await
        .unwrap();
    assert!(matches!(third, RunClaim::Started(_)), "got {third:?}");
}

#[sqlx::test]
async fn list_runs_paginated_and_isolated(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "test").await;
    let schedule = store.create_schedule(sq_id, 1, 300, None).await.unwrap();

    for i in 0usize..5 {
        let run_id = seed_run(&store, schedule.id, sq_id, "q").await;
        store
            .finish_run(
                run_id,
                RunStatus::Success,
                (i as u64) * 100,
                Some(i),
                None,
                None,
                None,
            )
            .await
            .unwrap();
    }

    let all = store.list_runs(sq_id, 1, 100, 0).await.unwrap();
    assert_eq!(all.len(), 5);

    let page = store.list_runs(sq_id, 1, 2, 1).await.unwrap();
    assert_eq!(page.len(), 2);

    // Wrong user sees nothing.
    assert!(store.list_runs(sq_id, 2, 100, 0).await.unwrap().is_empty());
    assert_eq!(store.count_runs_for_saved_query(sq_id, 1).await.unwrap(), 5);
    assert_eq!(store.count_runs_for_saved_query(sq_id, 2).await.unwrap(), 0);
}

#[sqlx::test]
async fn cleanup_stale_runs_marks_error(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "test").await;
    let schedule = store.create_schedule(sq_id, 1, 300, None).await.unwrap();

    // Start a run but don't finish it (simulates crash).
    seed_run(&store, schedule.id, sq_id, "q").await;

    let cleaned = store.cleanup_stale_runs().await.unwrap();
    assert_eq!(cleaned, 1);

    let runs = store.list_runs(sq_id, 1, 100, 0).await.unwrap();
    assert_eq!(runs[0].status, RunStatus::Error);
    assert_eq!(
        runs[0].error_message.as_deref(),
        Some("interrupted by server restart")
    );
}

#[sqlx::test]
async fn latest_run_returns_newest(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "test").await;
    let schedule = store.create_schedule(sq_id, 1, 300, None).await.unwrap();

    assert!(store.latest_run(schedule.id).await.unwrap().is_none());

    let run_id = seed_run(&store, schedule.id, sq_id, "q1").await;
    store
        .finish_run(run_id, RunStatus::Success, 100, None, None, None, None)
        .await
        .unwrap();

    let run_id = seed_run(&store, schedule.id, sq_id, "q2").await;
    store
        .finish_run(run_id, RunStatus::Error, 50, None, Some("boom"), None, None)
        .await
        .unwrap();

    let latest = store.latest_run(schedule.id).await.unwrap().unwrap();
    assert_eq!(latest.status, RunStatus::Error);
    assert_eq!(latest.query, "q2");
}

#[sqlx::test]
async fn list_enabled_schedules_filters_disabled(pool: PgPool) {
    let store = schedules(&pool);
    let sq1 = seed_saved(&pool, 1, "enabled-query").await;
    let sq2 = seed_saved(&pool, 2, "disabled-query").await;

    store.create_schedule(sq1, 1, 300, None).await.unwrap();
    let disabled = store.create_schedule(sq2, 2, 600, None).await.unwrap();
    store
        .update_schedule(disabled.id, 2, 600, None, false)
        .await
        .unwrap();

    let enabled = store.list_enabled_schedules().await.unwrap();
    assert_eq!(enabled.len(), 1);
    assert_eq!(enabled[0].1.name, "enabled-query");
    assert_eq!(store.count_enabled_schedules().await.unwrap(), 1);
}

#[sqlx::test]
async fn list_all_runs_paginated_and_isolated(pool: PgPool) {
    let store = schedules(&pool);
    let sq1 = seed_saved(&pool, 1, "alpha").await;
    let sq2 = seed_saved(&pool, 1, "beta").await;
    let sq3 = seed_saved(&pool, 2, "other-user").await;
    let sched1 = store.create_schedule(sq1, 1, 300, None).await.unwrap();
    let sched2 = store.create_schedule(sq2, 1, 300, None).await.unwrap();
    let sched3 = store.create_schedule(sq3, 2, 300, None).await.unwrap();

    for _ in 0..3 {
        let rid = seed_run(&store, sched1.id, sq1, "q1").await;
        store
            .finish_run(rid, RunStatus::Success, 50, None, None, None, None)
            .await
            .unwrap();
    }
    for _ in 0..2 {
        let rid = seed_run(&store, sched2.id, sq2, "q2").await;
        store
            .finish_run(rid, RunStatus::Success, 80, None, None, None, None)
            .await
            .unwrap();
    }
    let rid = seed_run(&store, sched3.id, sq3, "q3").await;
    store
        .finish_run(rid, RunStatus::Success, 10, None, None, None, None)
        .await
        .unwrap();

    let all = store.list_all_runs(1, 100, 0).await.unwrap();
    assert_eq!(all.len(), 5);
    let names: Vec<&str> = all.iter().map(|(_, name)| name.as_str()).collect();
    assert!(names.contains(&"alpha"));
    assert!(names.contains(&"beta"));
    assert!(!names.contains(&"other-user"));

    assert_eq!(store.list_all_runs(1, 2, 0).await.unwrap().len(), 2);
    assert_eq!(store.list_all_runs(1, 2, 2).await.unwrap().len(), 2);
    assert_eq!(store.count_all_runs(1).await.unwrap(), 5);
    assert_eq!(store.count_all_runs(2).await.unwrap(), 1);
}

#[sqlx::test]
async fn runs_stats_counts_by_status(pool: PgPool) {
    let store = schedules(&pool);

    // Empty first.
    let (total, success, error, timeout, avg) = store.runs_stats(1).await.unwrap();
    assert_eq!((total, success, error, timeout, avg), (0, 0, 0, 0, None));

    let sq_id = seed_saved(&pool, 1, "stats-test").await;
    let sched = store.create_schedule(sq_id, 1, 300, None).await.unwrap();

    for (status, ms, err) in [
        (RunStatus::Success, 100, None),
        (RunStatus::Success, 200, None),
        (RunStatus::Error, 50, Some("boom")),
        (RunStatus::Timeout, 300, Some("timed out")),
    ] {
        let rid = seed_run(&store, sched.id, sq_id, "q").await;
        store
            .finish_run(rid, status, ms, None, err, None, None)
            .await
            .unwrap();
    }

    let (total, success, error, timeout, avg) = store.runs_stats(1).await.unwrap();
    assert_eq!(total, 4);
    assert_eq!(success, 2);
    assert_eq!(error, 1);
    assert_eq!(timeout, 1);
    // avg of 100, 200, 50, 300 = 162.5 → 162 truncated
    assert_eq!(avg, Some(162));

    // Other user sees nothing.
    let (total, ..) = store.runs_stats(2).await.unwrap();
    assert_eq!(total, 0);
}

/// Retention: keeps at most `max_per_schedule` runs, deletes older than the
/// age cutoff, and returns the parquet paths of everything it deleted.
#[sqlx::test]
async fn delete_old_runs_retention_and_paths(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "retention").await;
    let sched = store.create_schedule(sq_id, 1, 300, None).await.unwrap();

    // Four finished runs with paths; backdate the first two beyond the cutoff.
    let mut run_ids = Vec::new();
    for i in 0..4 {
        let rid = seed_run(&store, sched.id, sq_id, "q").await;
        store
            .finish_run(
                rid,
                RunStatus::Success,
                10,
                Some(1),
                None,
                None,
                Some(&format!("scheduled/retention/run_{i}.parquet")),
            )
            .await
            .unwrap();
        run_ids.push(rid);
    }
    for rid in &run_ids[..2] {
        sqlx::query("UPDATE report_runs SET started_at = now() - INTERVAL '40 days' WHERE id = $1")
            .bind(rid)
            .execute(&pool)
            .await
            .unwrap();
    }

    // Age cutoff 30 days, and keep at most 1 per schedule.
    let (deleted, mut paths) = store.delete_old_runs(30, 1).await.unwrap();
    assert_eq!(deleted, 3, "two aged out + one excess");
    paths.sort();
    assert_eq!(
        paths,
        vec![
            "scheduled/retention/run_0.parquet".to_string(),
            "scheduled/retention/run_1.parquet".to_string(),
            "scheduled/retention/run_2.parquet".to_string(),
        ]
    );

    let remaining = store.list_runs(sq_id, 1, 100, 0).await.unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(
        remaining[0].result_path.as_deref(),
        Some("scheduled/retention/run_3.parquet")
    );
}

#[sqlx::test]
async fn successful_run_selectors(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "sel").await;
    let sched = store.create_schedule(sq_id, 1, 300, None).await.unwrap();

    // error run (no path), success with path, success without path.
    let r1 = seed_run(&store, sched.id, sq_id, "q").await;
    store
        .finish_run(r1, RunStatus::Error, 10, None, Some("x"), None, None)
        .await
        .unwrap();
    let r2 = seed_run(&store, sched.id, sq_id, "q").await;
    store
        .finish_run(
            r2,
            RunStatus::Success,
            10,
            Some(1),
            None,
            None,
            Some("p/run_2.parquet"),
        )
        .await
        .unwrap();
    let r3 = seed_run(&store, sched.id, sq_id, "q").await;
    store
        .finish_run(
            r3,
            RunStatus::Success,
            10,
            Some(1),
            None,
            Some(b"blob"),
            None,
        )
        .await
        .unwrap();

    let latest = store.latest_successful_run(sq_id).await.unwrap().unwrap();
    assert_eq!(latest.id, r2, "latest successful WITH a parquet path");

    let all = store.list_successful_runs(sq_id).await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, r2);
}

// ---------------------------------------------------------------------------
// concurrency: nothing serialises these calls in-process, so every
// atomicity claim has to be a postgres one.
// ---------------------------------------------------------------------------

/// Two connections racing `claim_run` on one schedule: exactly one claims
/// (the partial unique index `report_runs_one_running` decides).
#[sqlx::test]
async fn concurrent_start_run_yields_exactly_one_claim(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "race").await;
    let sched = store.create_schedule(sq_id, 1, 300, None).await.unwrap();

    let s1 = store.clone();
    let s2 = store.clone();
    let (a, b) = tokio::join!(
        tokio::spawn(async move { s1.claim_run(sched.id, sq_id, "q", None).await }),
        tokio::spawn(async move { s2.claim_run(sched.id, sq_id, "q", None).await }),
    );
    let a = a.unwrap().unwrap();
    let b = b.unwrap().unwrap();

    let a_started = matches!(a, RunClaim::Started(_));
    let b_started = matches!(b, RunClaim::Started(_));
    assert!(
        a_started ^ b_started,
        "exactly one racer must claim the run: got {a:?} / {b:?}"
    );
    assert_eq!(store.count_runs(sched.id).await.unwrap(), 1);
}

/// Concurrent manual triggers cannot exceed `max_runs`: the check and the
/// claim share one transaction serialized by FOR UPDATE on the schedule row.
#[sqlx::test]
async fn concurrent_claims_respect_max_runs(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "cap").await;
    let sched = store.create_schedule(sq_id, 1, 300, Some(2)).await.unwrap();

    // One finished run: exactly one claim slot remains under max_runs = 2.
    let rid = seed_run(&store, sched.id, sq_id, "q").await;
    store
        .finish_run(rid, RunStatus::Success, 10, None, None, None, None)
        .await
        .unwrap();

    let s1 = store.clone();
    let s2 = store.clone();
    let (a, b) = tokio::join!(
        tokio::spawn(async move { s1.claim_run(sched.id, sq_id, "q", Some(2)).await }),
        tokio::spawn(async move { s2.claim_run(sched.id, sq_id, "q", Some(2)).await }),
    );
    let a = a.unwrap().unwrap();
    let b = b.unwrap().unwrap();

    let started = [a, b]
        .iter()
        .filter(|c| matches!(c, RunClaim::Started(_)))
        .count();
    assert_eq!(started, 1, "one claim wins, one is refused: {a:?} / {b:?}");
    assert_eq!(
        store.count_runs(sched.id).await.unwrap(),
        2,
        "total runs never exceed max_runs"
    );
}

/// `finish_run` racing a cascade delete updates zero rows and reports it so
/// the caller can remove the file it just wrote.
#[sqlx::test]
async fn finish_run_after_cascade_delete_reports_orphan(pool: PgPool) {
    let saved_store = saved(&pool);
    let store = schedules(&pool);
    let sq = saved_store.create(1, "midflight", "q").await.unwrap();
    let sched = store.create_schedule(sq.id, 1, 300, None).await.unwrap();

    let rid = seed_run(&store, sched.id, sq.id, "q").await;

    // Cascade-delete the run mid-flight.
    saved_store.delete(sq.id, 1).await.unwrap();

    let updated = store
        .finish_run(
            rid,
            RunStatus::Success,
            10,
            Some(1),
            None,
            None,
            Some("scheduled/midflight/run.parquet"),
        )
        .await
        .unwrap();
    assert_eq!(
        updated,
        FinishOutcome::RunDeleted,
        "zero-row finish must be reported so the orphan file gets removed"
    );
}

/// `fail_run_if_running` is the ambiguous-commit recovery guard: it flips a
/// still-`running` row to `error`, and must leave a run whose success already
/// committed alone. An unconditional retry would destroy the committed result
/// and orphan its parquet file.
#[sqlx::test]
async fn fail_run_if_running_is_a_guarded_transition(pool: PgPool) {
    let saved_store = saved(&pool);
    let store = schedules(&pool);

    // A still-running run flips to error and the result path is cleared.
    let sq = saved_store.create(1, "running", "q").await.unwrap();
    let sched = store.create_schedule(sq.id, 1, 300, None).await.unwrap();
    let rid = seed_run(&store, sched.id, sq.id, "q").await;

    assert_eq!(
        store
            .fail_run_if_running(rid, 5, "result persistence failed")
            .await
            .unwrap(),
        FlipOutcome::FlippedToError,
        "a running row must be flipped to error"
    );
    let run = store.get_run(rid, 1).await.unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Error);
    assert_eq!(run.result_path, None);

    // A run whose success already committed must survive the guarded flip:
    // the ambiguous-commit case where the first finish_run's COMMIT landed.
    let sq2 = saved_store.create(1, "committed", "q").await.unwrap();
    let sched2 = store.create_schedule(sq2.id, 1, 300, None).await.unwrap();
    let rid2 = seed_run(&store, sched2.id, sq2.id, "q").await;
    assert_eq!(
        store
            .finish_run(
                rid2,
                RunStatus::Success,
                10,
                Some(7),
                None,
                None,
                Some("scheduled/committed/run.parquet"),
            )
            .await
            .unwrap(),
        FinishOutcome::Persisted
    );

    assert_eq!(
        store
            .fail_run_if_running(rid2, 5, "result persistence failed")
            .await
            .unwrap(),
        FlipOutcome::NotRunning,
        "a committed success must NOT be flipped (guard matches zero rows)"
    );
    let survived = store.get_run(rid2, 1).await.unwrap().unwrap();
    assert_eq!(
        survived.status,
        RunStatus::Success,
        "committed success preserved"
    );
    assert_eq!(survived.row_count, Some(7), "row_count preserved");
    assert_eq!(
        survived.result_path.as_deref(),
        Some("scheduled/committed/run.parquet"),
        "result_path preserved so the parquet file is not orphaned"
    );
}

/// A scheduler's `finish_run` landing its parquet path while
/// `SavedQueryStore::delete` is mid-flight must not orphan the file.
///
/// A blocker transaction pins the interleaving to the dangerous window, where
/// `finish_run` commits its path after `delete` has collected paths but before
/// its cascade wipes the row. `delete` locks the parent and every run row
/// first, so it either collects the path or the run row survives long enough
/// for `finish_run` to report the orphan. The invariant: `delete` returns the
/// path iff `finish_run` succeeded, so exactly one side owns the cleanup.
#[sqlx::test]
async fn delete_racing_finish_run_never_orphans_path(pool: PgPool) {
    const PATH: &str = "scheduled/race/run.parquet";
    let saved_store = saved(&pool);
    let sched_store = schedules(&pool);
    let sq = saved_store.create(1, "race", "q").await.unwrap();
    let sched = sched_store
        .create_schedule(sq.id, 1, 300, None)
        .await
        .unwrap();
    let rid = seed_run(&sched_store, sched.id, sq.id, "q").await;

    // Hold the parent row so `delete` stalls on its parent `FOR UPDATE`, the
    // spot the race needs.
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query_scalar::<_, i64>("SELECT id FROM saved_queries WHERE id = $1 FOR UPDATE")
        .bind(sq.id)
        .fetch_one(&mut *blocker)
        .await
        .unwrap();

    let delete_store = saved_store.clone();
    let sq_id = sq.id;
    let delete_task = tokio::spawn(async move { delete_store.delete(sq_id, 1).await });

    // Let `delete` reach its blocking point before the scheduler commits its
    // result path.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    let finished = sched_store
        .finish_run(rid, RunStatus::Success, 10, Some(1), None, None, Some(PATH))
        .await
        .unwrap();

    blocker.rollback().await.unwrap();
    let delete_paths = delete_task.await.unwrap().unwrap();

    assert_eq!(
        finished,
        FinishOutcome::Persisted,
        "finish_run committed its path before the cascade — it must report success"
    );
    assert_eq!(
        delete_paths.contains(&PATH.to_string()),
        finished == FinishOutcome::Persisted,
        "exactly one side must own the parquet cleanup: delete returned {delete_paths:?}"
    );
    assert_eq!(sched_store.count_runs(sched.id).await.unwrap(), 0);
}

/// The same race for `ScheduleStore::delete_schedule`: the blocker holds the
/// schedule row, `finish_run` commits its path mid-delete, and
/// `delete_schedule` must collect that path rather than cascade it away while
/// `finish_run` believes it persisted.
#[sqlx::test]
async fn delete_schedule_racing_finish_run_never_orphans_path(pool: PgPool) {
    const PATH: &str = "scheduled/race/sched.parquet";
    let sched_store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "race").await;
    let sched = sched_store
        .create_schedule(sq_id, 1, 300, None)
        .await
        .unwrap();
    let rid = seed_run(&sched_store, sched.id, sq_id, "q").await;

    let mut blocker = pool.begin().await.unwrap();
    sqlx::query_scalar::<_, i64>(
        "SELECT id FROM schedules WHERE saved_query_id = $1 AND key_id = $2 FOR UPDATE",
    )
    .bind(sq_id)
    .bind(1_i64)
    .fetch_one(&mut *blocker)
    .await
    .unwrap();

    let delete_store = sched_store.clone();
    let delete_task = tokio::spawn(async move { delete_store.delete_schedule(sq_id, 1).await });

    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    let finished = sched_store
        .finish_run(rid, RunStatus::Success, 10, Some(1), None, None, Some(PATH))
        .await
        .unwrap();

    blocker.rollback().await.unwrap();
    let delete_paths = delete_task.await.unwrap().unwrap();

    assert_eq!(
        finished,
        FinishOutcome::Persisted,
        "finish_run committed its path before the cascade — it must report success"
    );
    assert_eq!(
        delete_paths.contains(&PATH.to_string()),
        finished == FinishOutcome::Persisted,
        "exactly one side must own the parquet cleanup: delete_schedule returned {delete_paths:?}"
    );
    assert_eq!(sched_store.count_runs(sched.id).await.unwrap(), 0);
}

// ---------------------------------------------------------------------------
// boot: advisory lock + migration
// ---------------------------------------------------------------------------

/// Boot one `StorageState` on `url` through the real boot path.
///
/// `StorageState::from_pool` is that path: it takes the advisory lock on
/// its own session, then migrates. The only thing `StorageState::connect`
/// adds is the pool, and its production ceiling of 8 connections is more
/// than these tests hold at once (two live instances) has any use for. The
/// constructor itself stays under test in
/// `boot_unreachable_database_fails_descriptively`.
async fn boot(url: &str) -> Result<StorageState, StoreError> {
    StorageState::from_pool(common::app_pool(url).await).await
}

/// Fresh empty database + the real boot path applies the schema
/// (advisory lock BEFORE migrate).
#[tokio::test]
async fn boot_migrates_fresh_database() {
    let url = common::create_app_database().await;
    let storage = boot(&url).await.expect("first boot");

    // Schema is live: a store call succeeds.
    storage.saved.create(1, "boot", "q").await.unwrap();
    storage.ping().await.unwrap();
}

/// A second boot against an already-migrated database is a no-op (after the
/// first instance released its advisory lock).
#[tokio::test]
async fn boot_is_idempotent_after_shutdown() {
    let url = common::create_app_database().await;
    {
        let storage = boot(&url).await.expect("first boot");
        storage.saved.create(1, "boot", "q").await.unwrap();
        drop(storage);
    }

    // The advisory lock releases when the first instance's session closes;
    // that is asynchronous, so retry briefly.
    let mut last_err = None;
    for _ in 0..50 {
        match boot(&url).await {
            Ok(storage) => {
                // Data survived; migration did not re-run destructively.
                let survived = storage.saved.get_by_name(1, "boot").await.unwrap();
                assert!(survived.is_some(), "second boot must keep existing data");
                return;
            }
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
    panic!("second boot never succeeded: {last_err:?}");
}

/// A second live instance on the same DSN fails startup on the advisory
/// lock with a descriptive error.
#[tokio::test]
async fn boot_second_live_instance_fails_on_advisory_lock() {
    let url = common::create_app_database().await;
    let _first = boot(&url).await.expect("first boot");

    let err = boot(&url)
        .await
        .expect_err("second live instance must fail");
    assert!(matches!(err, StoreError::LockHeld), "got: {err:?}");
    assert!(
        err.to_string().contains("advisory lock"),
        "error must name the advisory lock: {err}"
    );
}

/// The lock session is detached, so it never occupies a pool slot.
///
/// `from_pool` acquires the advisory-lock connection from the pool it was
/// handed — that is what makes the lock and the writes it guards provably
/// the same database — and then detaches it. A connection merely held
/// checked out would spend a slot for the process lifetime: on a pool of
/// one, the migration that runs right after would wait for a connection
/// that never comes back. Boot completing here, and the pool answering a
/// query afterwards, is the evidence that detach happened.
#[tokio::test]
async fn boot_lock_session_does_not_hold_a_pool_slot() {
    let url = common::create_app_database().await;
    let pool = common::fixture_pool(&url, 1).await;

    let storage = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        StorageState::from_pool(pool),
    )
    .await
    .expect("boot must not wait on a pool slot the lock session took")
    .expect("boot on a single-connection pool");

    // The pool refilled the detached slot: ordinary store traffic flows.
    storage.ping().await.unwrap();
    storage.saved.create(1, "detached", "q").await.unwrap();
}

/// An unreachable storage database is a descriptive startup failure.
#[tokio::test]
async fn boot_unreachable_database_fails_descriptively() {
    // The one boot case that must keep `StorageState::connect`: what it
    // proves is that trawld's OWN pool constructor fails fast and
    // descriptively. The DSN is dead, so the pool never opens a connection.
    let err = StorageState::connect("postgres://trawl:nope@127.0.0.1:1/trawl")
        .await
        .expect_err("dead DSN must fail");
    assert!(matches!(err, StoreError::Unavailable(_)), "got: {err:?}");
}

/// Losing the lock-holding session must be detected, and must free the lock
/// for a replacement: the guard against two live instances writing at once.
///
/// Terminating every backend on the app database kills the dedicated
/// advisory-lock connection (postgres releases the session lock) while the
/// database survives. The original instance's guard must flip its lock-lost
/// signal (and report unhealthy on `/health`), and a *replacement* instance
/// must then be able to acquire the freed lock — proving the original can no
/// longer be trusted as sole writer.
#[tokio::test]
async fn lock_loss_is_detected_and_frees_the_lock_for_a_replacement() {
    use std::time::Duration;

    let url = common::create_app_database().await;
    let first = boot(&url).await.expect("first boot");

    let mut lost = first.lock_lost();
    assert!(!*lost.borrow_and_update(), "lock is healthy at boot");
    first.ping_cached().await.expect("storage healthy at boot");

    // While the lock is held, a replacement cannot start.
    assert!(
        matches!(boot(&url).await, Err(StoreError::LockHeld)),
        "second live instance must be locked out while the lock is held"
    );

    // Kill the lock-holding session (database stays up).
    common::terminate_backends(&url).await;

    // The guard keepalive-probes on an interval, so allow a few cycles.
    tokio::time::timeout(Duration::from_secs(30), lost.wait_for(|v| *v))
        .await
        .expect("guard must detect the lost lock")
        .expect("lock-lost sender must stay alive");

    // `/health` now reports storage unhealthy even though the pool itself
    // could reconnect — the split-brain hazard is surfaced.
    assert!(
        first.ping_cached().await.is_err(),
        "health must report the lost lock"
    );

    // The freed lock lets a replacement instance acquire it, retrying until
    // postgres has finished releasing the terminated session's lock.
    let mut last_err = None;
    for _ in 0..50 {
        match boot(&url).await {
            Ok(_replacement) => return,
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    panic!("replacement never acquired the freed lock: {last_err:?}");
}

// ---------------------------------------------------------------------------
// field catalog
// ---------------------------------------------------------------------------

mod catalog {
    use super::*;
    use trawl_core::schema::{CanonicalType, ENVELOPE_TYPES};
    use trawl_server::store::{CatalogStore, ConflictServicePair, FieldConflict, PinProposal};

    fn catalog(pool: &PgPool) -> CatalogStore {
        CatalogStore::new(pool.clone())
    }

    fn proposal(field: &str, ty: CanonicalType) -> PinProposal {
        PinProposal {
            field: field.to_owned(),
            ty,
            pinned_from: "svc-a".to_owned(),
        }
    }

    #[sqlx::test]
    async fn migration_seed_matches_envelope_types(pool: PgPool) {
        // The migration seed must mirror trawl-core's ENVELOPE_TYPES exactly.
        let pins = catalog(&pool).load_pins().await.unwrap();
        for (field, ty) in ENVELOPE_TYPES {
            let pinned = pins.iter().find(|(f, _)| f == field);
            assert_eq!(
                pinned.map(|(_, t)| *t),
                Some(*ty),
                "envelope field {field} must be pre-seeded with its declared type"
            );
        }
        assert_eq!(
            pins.len(),
            ENVELOPE_TYPES.len(),
            "a fresh catalog holds exactly the declared envelope"
        );
        // `_severity` is pinned SEVERITY, the semantic type persisted under
        // its own catalog spelling, and no `severity`/`severity_text` row is
        // seeded: those are ordinary sender vocabulary.
        assert_eq!(
            pins.iter().find(|(f, _)| f == "_severity").map(|(_, t)| *t),
            Some(CanonicalType::Severity)
        );
        for gone in ["severity", "severity_text"] {
            assert!(
                !pins.iter().any(|(f, _)| f == gone),
                "{gone} left the envelope (ADR-0013 §1)"
            );
        }
        // `_producer` is server-stamped provenance, seeded VARCHAR by
        // migration 0011 so its closed vocabulary (http | syslog | trawld)
        // is never typed by inference.
        assert_eq!(
            pins.iter().find(|(f, _)| f == "_producer").map(|(_, t)| *t),
            Some(CanonicalType::Varchar)
        );
    }

    /// Migration 0010's DELETE is scoped to the rows migration 0002
    /// declared: a sender's own `severity` pin is ordinary data and survives
    /// the reshape, along with its observations and evidence.
    #[sqlx::test]
    async fn the_seed_reshape_leaves_a_senders_own_severity_pin_standing(pool: PgPool) {
        let store = catalog(&pool);
        // A sender pins `severity` itself, exactly as any custom field.
        let pinned = store
            .pin_missing(&[proposal("severity", CanonicalType::BigInt)])
            .await
            .unwrap();
        assert_eq!(pinned.get("severity"), Some(&CanonicalType::BigInt));

        // Re-running the reshape's own statements must not touch it.
        sqlx::query(
            "DELETE FROM field_types WHERE field IN ('severity', 'severity_text') \
             AND pinned_from = '_declared'",
        )
        .execute(&pool)
        .await
        .unwrap();

        let pins = store.load_pins().await.unwrap();
        assert_eq!(
            pins.iter().find(|(f, _)| f == "severity").map(|(_, t)| *t),
            Some(CanonicalType::BigInt),
            "a sender's pin is not the declared seed"
        );
    }

    /// Row counts for `field` in (`field_services`, `field_conflicts`,
    /// `field_conflict_stats`) — the three tables the reshape prunes.
    async fn evidence_of(pool: &PgPool, field: &str) -> [i64; 3] {
        let services = sqlx::query_scalar("SELECT count(*) FROM field_services WHERE field = $1")
            .bind(field)
            .fetch_one(pool)
            .await
            .unwrap();
        let conflicts = sqlx::query_scalar("SELECT count(*) FROM field_conflicts WHERE field = $1")
            .bind(field)
            .fetch_one(pool)
            .await
            .unwrap();
        let stats =
            sqlx::query_scalar("SELECT count(*) FROM field_conflict_stats WHERE field = $1")
                .bind(field)
                .fetch_one(pool)
                .await
                .unwrap();
        [services, conflicts, stats]
    }

    /// The declared envelope as of migration 0010, written out.
    ///
    /// A migration-boundary assertion has to name the envelope of its own
    /// moment. Deriving it from today's `ENVELOPE_TYPES`, even minus the
    /// fields added since, couples the frozen past to the living present:
    /// retyping an envelope field would silently rewrite what this test
    /// claims 0010 produced, and every later addition would need another
    /// subtraction here. sqlx migrations are immutable once merged, so this
    /// list is too.
    const ENVELOPE_AT_0010: &[(&str, CanonicalType)] = &[
        ("_time", CanonicalType::Timestamp),
        ("_ingested", CanonicalType::Timestamp),
        ("_raw", CanonicalType::Varchar),
        ("_repairs", CanonicalType::Varchar),
        ("_severity", CanonicalType::Severity),
        ("env", CanonicalType::Varchar),
        ("service", CanonicalType::Varchar),
        ("host", CanonicalType::Varchar),
        ("message", CanonicalType::Varchar),
    ];

    fn envelope_at_0010() -> impl Iterator<Item = &'static (&'static str, CanonicalType)> {
        ENVELOPE_AT_0010.iter()
    }

    /// Migration 0010 applied over a real pre-cutover catalog.
    ///
    /// The reshape test above starts from the already-migrated schema and
    /// replays one of 0010's statements by hand, which can only speak for
    /// forward behaviour. This one builds the state a live install upgrades
    /// from (migrations 0001-0009, the declared `severity`/`severity_text`
    /// seed with observations and conflict evidence, a sender-owned
    /// `_severity` pin from before the `_` prefix was sealed, an unrelated
    /// sender field, a stamped `conformed_at`) and then applies 0010 over it.
    /// sqlx migrations are immutable once merged, so this is the only window
    /// in which the conditional DELETEs can be proven to scope correctly, and
    /// the corpus a mis-scoped DELETE unstores is standing data.
    #[sqlx::test(migrations = false)]
    async fn applying_0010_over_a_0009_catalog_reshapes_only_the_declared_rows(pool: PgPool) {
        // The crate's real migration set, the same files
        // `StorageState::connect` runs, so neither half can drift.
        static MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");
        MIGRATIONS
            .run_to(9, &pool)
            .await
            .expect("apply 0001-0009 (the pre-cutover schema)");

        // Pre-cutover sender state: `_severity` was pinnable from ordinary
        // sender data, and `duration` is the bystander nothing may touch.
        sqlx::query(
            "INSERT INTO field_types (field, duckdb_type, pinned_from, pinned_at) VALUES
                 ('_severity', 'VARCHAR', 'legacy-svc', now() - interval '30 days'),
                 ('duration',  'BIGINT',  'svc-a',      now() - interval '30 days')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO field_services (field, service, row_count) VALUES
                 ('severity',      'nginx',      10),
                 ('severity_text', 'nginx',      10),
                 ('_severity',     'legacy-svc', 10),
                 ('duration',      'svc-a',      10)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO field_conflicts
                 (field, service, observed_type, expected_type, rows_nulled, samples) VALUES
                 ('severity',      'nginx',      'VARCHAR', 'BIGINT',  3, ARRAY['warn']),
                 ('severity_text', 'nginx',      'BIGINT',  'VARCHAR', 1, ARRAY['17']),
                 ('_severity',     'legacy-svc', 'BIGINT',  'VARCHAR', 2, ARRAY['9']),
                 ('duration',      'svc-a',      'VARCHAR', 'BIGINT',  5, ARRAY['fast'])",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO field_conflict_stats
                 (field, service, episodes, rows_nulled_total) VALUES
                 ('severity',      'nginx',      2, 3),
                 ('severity_text', 'nginx',      1, 1),
                 ('_severity',     'legacy-svc', 4, 2),
                 ('duration',      'svc-a',      6, 5)",
        )
        .execute(&pool)
        .await
        .unwrap();
        // A node that has already proven its corpus conformant, and already
        // backfilled its observations.
        sqlx::query(
            "UPDATE catalog_state SET conformed_at = now(), services_backfilled_at = now()",
        )
        .execute(&pool)
        .await
        .unwrap();

        MIGRATIONS
            .run_to(10, &pool)
            .await
            .expect("apply 0010 over the pre-cutover catalog");

        // The pin table: exactly the envelope 0010 declares (nine fields;
        // `_producer` is 0011's, proven in the sibling test below) plus the
        // bystander. `load_pins` parsing at all proves the widened CHECK
        // constraint and `CanonicalType::from_catalog` agree on SEVERITY.
        let pins = catalog(&pool).load_pins().await.unwrap();
        for (field, ty) in envelope_at_0010() {
            assert_eq!(
                pins.iter().find(|(f, _)| f == field).map(|(_, t)| *t),
                Some(*ty),
                "declared {field} must stand after the reshape"
            );
        }
        assert!(
            !pins.iter().any(|(f, _)| f == "_producer"),
            "`_producer` is 0011's, not 0010's: {pins:?}"
        );
        assert_eq!(
            pins.iter().find(|(f, _)| f == "duration").map(|(_, t)| *t),
            Some(CanonicalType::BigInt),
            "an unrelated sender pin is untouched"
        );
        assert_eq!(
            pins.len(),
            envelope_at_0010().count() + 1,
            "the reshape leaves the declared envelope plus the sender's own pin: {pins:?}"
        );
        // `_severity` is replaced, not merely present: the sender's VARCHAR
        // pin is gone and the row is the declared seed's.
        let pinned_from: Option<String> =
            sqlx::query_scalar("SELECT pinned_from FROM field_types WHERE field = '_severity'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(pinned_from.as_deref(), Some("_declared"));
        let bystander: Option<String> =
            sqlx::query_scalar("SELECT pinned_from FROM field_types WHERE field = 'duration'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(bystander.as_deref(), Some("svc-a"));

        // Evidence follows the pin it describes: the two retired declared
        // names and the replaced `_severity` lose theirs, the bystander
        // keeps all three rows.
        for gone in ["severity", "severity_text", "_severity"] {
            assert_eq!(
                evidence_of(&pool, gone).await,
                [0, 0, 0],
                "{gone}'s observations and conflict evidence go with its pin"
            );
        }
        assert_eq!(
            evidence_of(&pool, "duration").await,
            [1, 1, 1],
            "an unrelated field's evidence survives the reshape"
        );

        // Re-armed for epoch 3's fresh data root, and nothing else in
        // `catalog_state` is disturbed (a set-aside root has no standing
        // corpus to backfill observations from).
        let (conformed, backfilled): (
            Option<chrono::DateTime<chrono::Utc>>,
            Option<chrono::DateTime<chrono::Utc>>,
        ) = sqlx::query_as("SELECT conformed_at, services_backfilled_at FROM catalog_state")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(conformed.is_none(), "the conformance pass must be re-armed");
        assert!(
            backfilled.is_some(),
            "the observation backfill flag is not the reshape's business"
        );
    }

    /// Migration 0011 (`_producer`) applied over a catalog that already
    /// carries a sender-owned `_producer` pin.
    ///
    /// Such a pin can only date from before the `_` prefix was sealed, which
    /// is exactly the catalog a live install replays 0010 and 0011 over in one
    /// boot. 0011's DELETEs claim the name unconditionally, because the column
    /// is server-stamped and typing it from an old sender's data would misread
    /// a closed vocabulary. A merged migration is immutable, so this is the
    /// only window in which that scoping can be proven.
    #[sqlx::test(migrations = false)]
    async fn applying_0011_claims_producer_and_leaves_sender_pins_alone(pool: PgPool) {
        static MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");
        MIGRATIONS
            .run_to(9, &pool)
            .await
            .expect("apply 0001-0009 (the pre-cutover schema)");

        // The era when `_` was an ordinary character: a sender pinned
        // `_producer` from its own data, with observations and evidence.
        // `duration` is the bystander nothing may touch.
        sqlx::query(
            "INSERT INTO field_types (field, duckdb_type, pinned_from, pinned_at) VALUES
                 ('_producer', 'BIGINT', 'legacy-svc', now() - interval '30 days'),
                 ('duration',  'BIGINT', 'svc-a',      now() - interval '30 days')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO field_services (field, service, row_count) VALUES
                 ('_producer', 'legacy-svc', 10),
                 ('duration',  'svc-a',      10)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO field_conflicts
                 (field, service, observed_type, expected_type, rows_nulled, samples) VALUES
                 ('_producer', 'legacy-svc', 'VARCHAR', 'BIGINT', 2, ARRAY['edge-1']),
                 ('duration',  'svc-a',      'VARCHAR', 'BIGINT', 5, ARRAY['fast'])",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO field_conflict_stats (field, service, episodes, rows_nulled_total) VALUES
                 ('_producer', 'legacy-svc', 4, 2),
                 ('duration',  'svc-a',      6, 5)",
        )
        .execute(&pool)
        .await
        .unwrap();

        MIGRATIONS
            .run_to(11, &pool)
            .await
            .expect("apply 0010 and 0011 over the pre-cutover catalog");

        // The full declared envelope now stands, `_producer` included.
        let pins = catalog(&pool).load_pins().await.unwrap();
        for (field, ty) in ENVELOPE_TYPES {
            assert_eq!(
                pins.iter().find(|(f, _)| f == field).map(|(_, t)| *t),
                Some(*ty),
                "declared {field} must stand after 0011"
            );
        }
        assert_eq!(
            pins.len(),
            ENVELOPE_TYPES.len() + 1,
            "the ten declared fields plus the sender's own pin: {pins:?}"
        );

        // `_producer` is replaced, not merely present: the sender's BIGINT
        // pin is gone, the row is the declared seed's, and the evidence
        // describing the retired pin goes with it.
        let pinned_from: Option<String> =
            sqlx::query_scalar("SELECT pinned_from FROM field_types WHERE field = '_producer'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(pinned_from.as_deref(), Some("_declared"));
        assert_eq!(
            evidence_of(&pool, "_producer").await,
            [0, 0, 0],
            "the retired pin's observations and evidence go with it"
        );

        // The bystander is untouched in every table.
        assert_eq!(
            pins.iter().find(|(f, _)| f == "duration").map(|(_, t)| *t),
            Some(CanonicalType::BigInt)
        );
        assert_eq!(
            evidence_of(&pool, "duration").await,
            [1, 1, 1],
            "an unrelated field's evidence survives 0011"
        );
    }

    /// `_producer` is a new column on a corpus that never held one, so
    /// 0011 must not re-arm the boot conformance pass: `UNION ALL BY NAME`
    /// tolerates a column absent from older parquet, and a spurious
    /// re-arm would make every upgrading node re-walk its whole archive.
    #[sqlx::test(migrations = false)]
    async fn migration_0011_leaves_the_conformance_flags_alone(pool: PgPool) {
        static MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");
        MIGRATIONS.run_to(10, &pool).await.expect("apply 0001-0010");
        sqlx::query(
            "UPDATE catalog_state SET conformed_at = now(), services_backfilled_at = now()",
        )
        .execute(&pool)
        .await
        .unwrap();

        MIGRATIONS.run_to(11, &pool).await.expect("apply 0011");

        let (conformed, backfilled): (
            Option<chrono::DateTime<chrono::Utc>>,
            Option<chrono::DateTime<chrono::Utc>>,
        ) = sqlx::query_as("SELECT conformed_at, services_backfilled_at FROM catalog_state")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(
            conformed.is_some(),
            "adding a never-before-written column re-arms nothing"
        );
        assert!(backfilled.is_some());
    }

    #[sqlx::test]
    async fn pin_missing_is_first_writer_wins(pool: PgPool) {
        let store = catalog(&pool);

        let pins = store
            .pin_missing(&[proposal("duration", CanonicalType::BigInt)])
            .await
            .unwrap();
        assert_eq!(pins.get("duration"), Some(&CanonicalType::BigInt));

        // A later batch proposing a different type does not repin; the
        // authoritative pin comes back instead.
        let pins = store
            .pin_missing(&[proposal("duration", CanonicalType::Varchar)])
            .await
            .unwrap();
        assert_eq!(
            pins.get("duration"),
            Some(&CanonicalType::BigInt),
            "an existing pin is never overwritten by pin_missing"
        );
    }

    #[sqlx::test]
    async fn pin_missing_race_converges_on_one_pin(pool: PgPool) {
        // Two concurrent proposals for the same unpinned field with
        // different types: both callers must come back with the same
        // authoritative pin (INSERT ... ON CONFLICT DO NOTHING + re-read).
        let a = catalog(&pool);
        let b = catalog(&pool);
        let pa_prop = [proposal("racy", CanonicalType::BigInt)];
        let pb_prop = [proposal("racy", CanonicalType::Varchar)];
        let (ra, rb) = tokio::join!(a.pin_missing(&pa_prop), b.pin_missing(&pb_prop));
        let pa = ra.unwrap();
        let pb = rb.unwrap();
        assert_eq!(
            pa.get("racy"),
            pb.get("racy"),
            "both racers must observe the same authoritative pin"
        );
        assert!(pa.contains_key("racy"));
    }

    /// A name no btree key can hold: 3000 pseudo-random printable bytes.
    ///
    /// The wide alphabet is load-bearing. Postgres pglz-compresses an
    /// oversized index value before giving up, so a repeated-character
    /// name of the same length slips under the limit and would not
    /// exercise the failure at all; this one reproduces
    /// `index row size 3016 exceeds btree version 4 maximum 2704`
    /// verbatim against `field_types_pkey`.
    fn unstorable_field_name() -> String {
        let mut x: u32 = 0x1234_5678;
        std::iter::repeat_with(|| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            char::from(33 + u8::try_from((x >> 16) % 93).unwrap())
        })
        .take(3000)
        .collect()
    }

    #[sqlx::test]
    async fn pin_missing_skips_unstorable_names_instead_of_failing(pool: PgPool) {
        // An over-long field name must never error the pin statement:
        // pinning gates every parquet write, so an Err here retains the
        // batch and re-fails on every compaction tick, forever.
        let store = catalog(&pool);
        let huge = unstorable_field_name();

        let pins = store
            .pin_missing(&[
                proposal(&huge, CanonicalType::Varchar),
                proposal("duration", CanonicalType::BigInt),
            ])
            .await
            .expect("an unstorable name must not fail the batch's pins");

        assert!(
            !pins.contains_key(&huge),
            "the unstorable name is not pinned — leaving it unpinned is what \
             makes the conform step drop the column"
        );
        assert_eq!(
            pins.get("duration"),
            Some(&CanonicalType::BigInt),
            "every other field in the batch still pins"
        );

        // And it really is absent from the catalog, not silently stored.
        let all = store.load_pins().await.unwrap();
        assert!(all.iter().all(|(f, _)| f != &huge));
    }

    #[sqlx::test]
    async fn pin_missing_caps_catalog_cardinality(pool: PgPool) {
        // Field names are client-chosen JSON keys and the ingest path never
        // reclaims a pin (retention does not reconcile the catalog), so an
        // uncapped catalog is an unbounded postgres table and an unbounded
        // in-process cache that a sender embedding identifiers in its keys
        // grows for free. Only the free slots under the cap are filled; the
        // surplus stays unpinned, which is what makes the conform step drop
        // the column with its values still in `_raw`.
        let seeded = i64::try_from(ENVELOPE_TYPES.len()).unwrap();
        let store = catalog(&pool).with_pin_cap(seeded + 2);

        // Two free slots, and one batch may take at most half of them.
        let pins = store
            .pin_missing(&[
                proposal("a_field", CanonicalType::BigInt),
                proposal("b_field", CanonicalType::BigInt),
                proposal("c_field", CanonicalType::BigInt),
                proposal("d_field", CanonicalType::BigInt),
            ])
            .await
            .expect("a full catalog must never error the batch — retrying cannot clear it");

        assert_eq!(pins.len(), 1, "one batch takes at most half the free slots");
        assert!(
            pins.contains_key("a_field"),
            "an overflowing batch picks deterministically, by name"
        );

        // One free slot left: the ration bounds a burst, it never strands
        // the last slot.
        let pins = store
            .pin_missing(&[
                proposal("b_field", CanonicalType::BigInt),
                proposal("c_field", CanonicalType::BigInt),
            ])
            .await
            .unwrap();
        assert_eq!(pins.len(), 1, "the last free slot is still grantable");
        assert!(pins.contains_key("b_field"));

        let all = store.load_pins().await.unwrap();
        assert_eq!(
            i64::try_from(all.len()).unwrap(),
            seeded + 2,
            "the catalog never exceeds the cap"
        );

        // A later batch against a full catalog pins nothing new, but must
        // still resolve the pins that do exist, or every known column of
        // every batch would start being dropped once the cap is reached.
        let pins = store
            .pin_missing(&[
                proposal("e_field", CanonicalType::BigInt),
                proposal("a_field", CanonicalType::Varchar),
            ])
            .await
            .unwrap();
        assert_eq!(
            pins.get("a_field"),
            Some(&CanonicalType::BigInt),
            "existing pins still resolve at a full catalog"
        );
        assert!(!pins.contains_key("e_field"), "no new pin fits");
        assert_eq!(
            store.load_pins().await.unwrap().len(),
            all.len(),
            "a full catalog never grows"
        );
    }

    #[sqlx::test]
    async fn one_batch_cannot_consume_the_catalog_but_the_boot_seed_can(pool: PgPool) {
        // A pin slot is spent permanently on the ingest path and denial is
        // silent in the data: the column is simply absent from every later
        // parquet file. Under first-come-first-served, one ingest request
        // carrying enough junk keys would unstore every future field on the
        // install, from every service, so a batch takes at most half the free
        // slots and the tail survives a burst.
        let seeded = i64::try_from(ENVELOPE_TYPES.len()).unwrap();
        let store = catalog(&pool).with_pin_cap(seeded + 8);

        let burst: Vec<PinProposal> = (0..8)
            .map(|i| proposal(&format!("junk_{i:02}"), CanonicalType::BigInt))
            .collect();
        let pins = store.pin_missing(&burst).await.unwrap();
        assert_eq!(
            pins.len(),
            4,
            "half of the eight free slots, never all of them"
        );

        let pins = store
            .pin_missing(&[proposal("legit_field", CanonicalType::BigInt)])
            .await
            .unwrap();
        assert!(
            pins.contains_key("legit_field"),
            "a burst never leaves the next legitimate field homeless"
        );

        // The boot conformance pass is exempt: its proposals describe
        // columns already on disk, so a denied pin there deletes standing
        // data rather than declining to add a column.
        let pins = store.pin_missing_unrationed(&burst).await.unwrap();
        assert_eq!(
            pins.len(),
            7,
            "the seed fills every free slot the cap allows"
        );
        assert_eq!(
            i64::try_from(store.load_pins().await.unwrap().len()).unwrap(),
            seeded + 8,
            "and the cap still holds absolutely"
        );
    }

    #[sqlx::test]
    async fn touch_services_skips_unstorable_names_instead_of_failing(pool: PgPool) {
        let store = catalog(&pool);
        let huge = unstorable_field_name();

        store
            .touch_services("svc-a", &[huge.clone(), "duration".to_owned()], 7)
            .await
            .expect("an unstorable name must not fail the observation upsert");

        assert!(
            store
                .field_services(&huge, None, 1000)
                .await
                .unwrap()
                .0
                .is_empty()
        );
        assert_eq!(
            store
                .field_services("duration", None, 1000)
                .await
                .unwrap()
                .0
                .len(),
            1
        );
    }

    #[sqlx::test]
    async fn touch_services_advances_only_last_seen(pool: PgPool) {
        let store = catalog(&pool);
        let fields = vec!["duration".to_owned()];

        store.touch_services("svc-a", &fields, 10).await.unwrap();
        let first = store
            .field_services("duration", None, 1000)
            .await
            .unwrap()
            .0;
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].service, "svc-a");
        assert_eq!(first[0].row_count, 10);
        assert_eq!(first[0].first_seen, first[0].last_seen);

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        store.touch_services("svc-a", &fields, 5).await.unwrap();
        let second = store
            .field_services("duration", None, 1000)
            .await
            .unwrap()
            .0;
        assert_eq!(second.len(), 1, "upsert, not append");
        assert_eq!(second[0].row_count, 15, "row_count accumulates");
        assert_eq!(
            second[0].first_seen, first[0].first_seen,
            "first_seen never moves"
        );
        assert!(
            second[0].last_seen > first[0].last_seen,
            "last_seen advances"
        );
    }

    #[sqlx::test]
    async fn touch_services_is_ever_observed_no_eviction(pool: PgPool) {
        // `field_services` rows are ever-observed: no window, no eviction, so
        // a service observed once stays observed however many other services
        // later carry the field. Consumers window on `last_seen`.
        let store = catalog(&pool);
        let fields = vec!["duration".to_owned()];

        for name in ["svc-a", "svc-b", "svc-c", "svc-d", "svc-e"] {
            store.touch_services(name, &fields, 1).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let rows = store
            .field_services("duration", None, 1000)
            .await
            .unwrap()
            .0;
        let kept: Vec<&str> = rows.iter().map(|r| r.service.as_str()).collect();
        assert_eq!(
            kept,
            vec!["svc-e", "svc-d", "svc-c", "svc-b", "svc-a"],
            "every service ever observed is still there, most recent first"
        );
    }

    /// The service axis is client-chosen and never pruned, so the read
    /// surface pages it: a cursor walk over a large history delivers every
    /// row exactly once, in order, and terminates.
    #[sqlx::test]
    async fn field_services_pages_a_large_history_without_gaps(pool: PgPool) {
        let store = catalog(&pool);

        // 250 services carrying one common field — no pin slot spent, and
        // nothing ever removes a row.
        sqlx::query(
            "INSERT INTO field_services (field, service, first_seen, last_seen, row_count)
             SELECT 'duration', 'svc-' || lpad(g::text, 4, '0'),
                    now() - interval '1 day', now() - (g || ' seconds')::interval, 1
             FROM generate_series(1, 250) g",
        )
        .execute(&pool)
        .await
        .unwrap();

        let mut seen: Vec<String> = Vec::new();
        let mut cursor = None;
        let mut pages = 0;
        loop {
            let (rows, next) = store
                .field_services("duration", cursor.as_ref(), 60)
                .await
                .unwrap();
            assert!(rows.len() <= 60, "a page never exceeds its limit");
            pages += 1;
            assert!(pages <= 10, "the walk must terminate");
            seen.extend(rows.iter().map(|r| r.service.clone()));
            match next {
                Some(c) => {
                    assert_eq!(
                        c.service,
                        rows.last().unwrap().service,
                        "the cursor names the last row delivered"
                    );
                    cursor = Some(c);
                }
                None => break,
            }
        }

        assert_eq!(pages, 5, "250 rows at 60 per page");
        assert_eq!(seen.len(), 250, "every row delivered");
        let mut unique = seen.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 250, "no row delivered twice: {seen:?}");
        assert_eq!(
            seen.first().unwrap(),
            "svc-0001",
            "most recent observation first"
        );
        assert_eq!(seen.last().unwrap(), "svc-0250", "oldest last");
    }

    /// Rows sharing a `last_seen` still page deterministically: the cursor
    /// is `(last_seen, service)`, so the tie breaks on the service name.
    #[sqlx::test]
    async fn field_services_cursor_breaks_last_seen_ties(pool: PgPool) {
        let store = catalog(&pool);
        sqlx::query(
            "INSERT INTO field_services (field, service, first_seen, last_seen, row_count)
             SELECT 'duration', 'svc-' || g, now() - interval '1 day', now(), 1
             FROM generate_series(1, 5) g",
        )
        .execute(&pool)
        .await
        .unwrap();

        let (first, next) = store.field_services("duration", None, 2).await.unwrap();
        let names: Vec<&str> = first.iter().map(|r| r.service.as_str()).collect();
        assert_eq!(names, vec!["svc-1", "svc-2"]);
        let (second, _) = store
            .field_services("duration", next.as_ref(), 2)
            .await
            .unwrap();
        let names: Vec<&str> = second.iter().map(|r| r.service.as_str()).collect();
        assert_eq!(names, vec!["svc-3", "svc-4"], "no repeat across the tie");
    }

    #[sqlx::test]
    async fn conflicts_append_and_never_aggregate(pool: PgPool) {
        let store = catalog(&pool);
        let row = FieldConflict {
            field: "duration".to_owned(),
            service: "svc-b".to_owned(),
            observed_type: "VARCHAR".to_owned(),
            expected_type: CanonicalType::BigInt,
            rows_nulled: 3,
            samples: vec!["n/a".to_owned()],
        };
        store
            .record_conflicts(std::slice::from_ref(&row))
            .await
            .unwrap();
        store.record_conflicts(&[row]).await.unwrap();

        let rows = store.conflicts_for_field("duration").await.unwrap();
        assert_eq!(rows.len(), 2, "field_conflicts is append-only");
        assert_eq!(rows[0].service, "svc-b");
        assert_eq!(rows[0].observed_type, "VARCHAR");
        assert_eq!(rows[0].expected_type, "BIGINT");
        assert_eq!(rows[0].rows_nulled, 3);
        assert_eq!(
            rows[0].samples,
            vec!["n/a".to_owned()],
            "the misfit samples ride the row they annotate"
        );
    }

    /// The samples are per-row evidence: an arbitrary list of client text
    /// per conflict, carried whole (empty included) rather than merged.
    #[sqlx::test]
    async fn conflict_samples_round_trip_per_row(pool: PgPool) {
        let store = catalog(&pool);
        let mk = |service: &str, samples: Vec<String>| FieldConflict {
            field: "duration".to_owned(),
            service: service.to_owned(),
            observed_type: "VARCHAR".to_owned(),
            expected_type: CanonicalType::BigInt,
            rows_nulled: 1,
            samples,
        };
        // Postgres array literals, JSON and SQL all have opinions about
        // these characters; the value that arrives is the value that was
        // captured.
        let awkward = vec![
            "n/a".to_owned(),
            "{\"a\",\"b\"}".to_owned(),
            "he said \"no\"".to_owned(),
            "back\\slash".to_owned(),
            "\u{fffd}\u{1f600}".to_owned(),
        ];
        store
            .record_conflicts(&[mk("svc-a", awkward.clone()), mk("svc-b", Vec::new())])
            .await
            .unwrap();

        let rows = store.conflicts_for_field("duration").await.unwrap();
        let by_service: std::collections::HashMap<&str, &Vec<String>> = rows
            .iter()
            .map(|r| (r.service.as_str(), &r.samples))
            .collect();
        assert_eq!(by_service["svc-a"], &awkward);
        assert!(
            by_service["svc-b"].is_empty(),
            "a lane with no values in hand records none"
        );

        let (listed, _) = store
            .recent_conflicts(Some("duration"), Some("svc-a"), None, 10)
            .await
            .unwrap();
        assert_eq!(
            listed[0].samples, awkward,
            "the cross-field listing carries the same evidence"
        );
    }

    /// The verdict's evidence read is bounded per field, not per page: one
    /// degraded field cannot spend the whole budget and leave the rest of
    /// the page verdictless, and a page of degraded fields cannot pull the
    /// whole evidence table into one response.
    #[sqlx::test]
    async fn conflict_evidence_is_bounded_per_field(pool: PgPool) {
        let store = catalog(&pool);
        for field in ["duration", "status"] {
            for i in 0..40_u64 {
                store
                    .record_conflicts(&[FieldConflict {
                        field: field.to_owned(),
                        service: "svc-a".to_owned(),
                        observed_type: "VARCHAR".to_owned(),
                        expected_type: CanonicalType::BigInt,
                        rows_nulled: 1,
                        samples: vec![format!("v{i}")],
                    }])
                    .await
                    .unwrap();
            }
        }

        let evidence = store
            .conflict_evidence_for(&["duration".to_owned(), "status".to_owned()])
            .await
            .unwrap();
        assert_eq!(evidence.len(), 2, "every field asked for gets evidence");
        for (field, rows) in &evidence {
            assert!(
                rows.len() <= 5,
                "{field}: {} rows exceeds the per-field bound",
                rows.len()
            );
            // Newest first: the last episode written is the first row back.
            assert_eq!(rows[0].1, vec!["v39".to_owned()], "{field}");
        }
    }

    /// The aggregates exist because the detail window does not survive a
    /// storm: `field_conflicts` is trimmed to the cap in the same
    /// transaction that appends to it, while `field_conflict_stats` keeps
    /// the span, the episode count and the lifetime shelved rows the
    /// analyzer judges the pin on.
    #[sqlx::test]
    async fn conflict_stats_outlive_the_recency_trim(pool: PgPool) {
        let store = catalog(&pool).with_conflict_cap(10);
        for i in 0..60_u64 {
            let service = if i % 2 == 0 { "svc-a" } else { "svc-b" };
            store
                .record_conflicts(&[FieldConflict {
                    field: "duration".to_owned(),
                    service: service.to_owned(),
                    observed_type: "VARCHAR".to_owned(),
                    expected_type: CanonicalType::BigInt,
                    rows_nulled: 2,
                    samples: vec!["n/a".to_owned()],
                }])
                .await
                .unwrap();
        }

        assert_eq!(
            store.conflicts_for_field("duration").await.unwrap().len(),
            10,
            "the detail window is at the cap"
        );
        let stats: Vec<(String, i64, i64)> = sqlx::query_as(
            "SELECT service, episodes, rows_nulled_total FROM field_conflict_stats
             WHERE field = $1 ORDER BY service",
        )
        .bind("duration")
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            stats,
            vec![("svc-a".to_owned(), 30, 60), ("svc-b".to_owned(), 30, 60)],
            "every episode is counted, per sender, whatever the window kept"
        );
        let span: bool = sqlx::query_scalar(
            "SELECT bool_and(last_at >= first_at) FROM field_conflict_stats WHERE field = $1",
        )
        .bind("duration")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(span, "the span is the evidence, and it only widens");
    }

    /// The degraded generation's two reads share one postgres snapshot.
    ///
    /// Both halves read `field_conflict_stats`, and a repin's evidence clear
    /// deletes from it. As two pool reads (READ COMMITTED gives a fresh
    /// snapshot per statement) a clear landing between them would publish a
    /// generation with a degraded field and no service attributed to it: the
    /// query notice stands while every badge vanishes, for a whole refresh
    /// interval.
    ///
    /// The isolation level is only observable by interleaving a committed
    /// delete from a second connection, so the test drives the two
    /// transaction-scoped reads `CatalogStore::degraded_snapshot` composes.
    #[sqlx::test]
    async fn degraded_snapshot_reads_survive_a_concurrent_evidence_clear(pool: PgPool) {
        let store = catalog(&pool);
        let episode = |service: &str| FieldConflict {
            field: "duration".to_owned(),
            service: service.to_owned(),
            observed_type: "VARCHAR".to_owned(),
            expected_type: CanonicalType::BigInt,
            rows_nulled: 3,
            samples: vec!["n/a".to_owned()],
        };
        for _ in 0..2 {
            store
                .record_conflicts(&[episode("svc-a"), episode("svc-b")])
                .await
                .unwrap();
        }
        // The span half of the degrade gate is the one thing a test cannot
        // wait 24 hours for.
        sqlx::query(
            "UPDATE field_conflict_stats SET first_at = now() - interval '48 hours'
             WHERE field = 'duration'",
        )
        .execute(&pool)
        .await
        .unwrap();
        let visible = vec!["svc-a".to_owned(), "svc-b".to_owned()];

        // Baseline: the composed read gives both halves of one generation.
        let (degraded, pairs) = store.degraded_snapshot(&visible).await.unwrap();
        assert!(degraded.contains("duration"), "the pin is degraded");
        assert_eq!(
            pairs,
            vec![
                ConflictServicePair {
                    field: "duration".to_owned(),
                    service: "svc-a".to_owned(),
                },
                ConflictServicePair {
                    field: "duration".to_owned(),
                    service: "svc-b".to_owned(),
                },
            ],
            "both senders are attributed"
        );

        // Now the interleave the transaction exists for.
        let mut tx = store.begin_evidence_snapshot().await.unwrap();
        let aggregates = CatalogStore::conflict_aggregates_tx(&mut tx, None)
            .await
            .unwrap();
        assert_eq!(aggregates.len(), 1, "the first half read the evidence");

        let mut other = pool.acquire().await.unwrap();
        CatalogStore::clear_conflict_evidence(&mut other, "duration")
            .await
            .unwrap();
        assert_eq!(
            store.conflict_aggregates(None).await.unwrap().len(),
            0,
            "the clear really committed — a read outside the transaction sees it gone"
        );

        let names = vec!["duration".to_owned()];
        let pairs = CatalogStore::conflict_service_pairs_tx(&mut tx, &names, &visible)
            .await
            .unwrap();
        assert_eq!(
            pairs.len(),
            2,
            "the second half sees the same table the first half did — \
             a torn generation would have degraded fields and zero pairs"
        );
        tx.commit().await.unwrap();

        // And the next generation is empty on both halves: keeping the reads
        // together does not keep evidence alive.
        let (degraded, pairs) = store.degraded_snapshot(&visible).await.unwrap();
        assert!(
            degraded.is_empty(),
            "the repinned field is no longer badged"
        );
        assert!(pairs.is_empty(), "and nothing is attributed to it");
    }

    /// `first_at` only ever moves earlier, the property the whole degraded
    /// gate rests on.
    ///
    /// The gate is `last_at - first_at >= 24h`. If a later episode restamped
    /// `first_at`, the span would reset on every conflict and no field could
    /// ever be called degraded: the feature would be inert, silently, with
    /// every other test still green.
    #[sqlx::test]
    async fn recording_an_episode_never_moves_first_at_forward(pool: PgPool) {
        let store = catalog(&pool);
        let episode = || FieldConflict {
            field: "duration".to_owned(),
            service: "svc-a".to_owned(),
            observed_type: "VARCHAR".to_owned(),
            expected_type: CanonicalType::BigInt,
            rows_nulled: 1,
            samples: Vec::new(),
        };
        store.record_conflicts(&[episode()]).await.unwrap();
        sqlx::query(
            "UPDATE field_conflict_stats SET first_at = now() - interval '48 hours'
             WHERE field = $1",
        )
        .bind("duration")
        .execute(&pool)
        .await
        .unwrap();

        store.record_conflicts(&[episode()]).await.unwrap();

        let (span_hours, episodes): (f64, i64) = sqlx::query_as(
            "SELECT (extract(epoch FROM (last_at - first_at)) / 3600.0)::float8, episodes
             FROM field_conflict_stats WHERE field = $1",
        )
        .bind("duration")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(episodes, 2, "the later episode was recorded");
        assert!(
            span_hours >= 47.9,
            "the span must still be the backdated distance, not reset: {span_hours}h"
        );
    }

    /// The boot conformance pass accumulates conflicts across every file it
    /// rewrites, so one call routinely carries many rows for the same
    /// `(field, service)`. Postgres refuses to let one `ON CONFLICT DO
    /// UPDATE` touch a row twice, so the upsert aggregates in the statement.
    #[sqlx::test]
    async fn conflict_stats_aggregate_duplicate_pairs_within_one_call(pool: PgPool) {
        let store = catalog(&pool);
        let batch: Vec<FieldConflict> = (0..4_u64)
            .map(|i| FieldConflict {
                field: "duration".to_owned(),
                service: "svc-a".to_owned(),
                observed_type: "VARCHAR".to_owned(),
                expected_type: CanonicalType::BigInt,
                rows_nulled: i,
                samples: Vec::new(),
            })
            .collect();
        store.record_conflicts(&batch).await.unwrap();

        let (episodes, nulled): (i64, i64) = sqlx::query_as(
            "SELECT episodes, rows_nulled_total FROM field_conflict_stats
             WHERE field = $1 AND service = $2",
        )
        .bind("duration")
        .bind("svc-a")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (episodes, nulled),
            (4, 6),
            "four rows in one call are four episodes, not one and not an error"
        );
        assert_eq!(
            store.conflicts_for_field("duration").await.unwrap().len(),
            4,
            "the detail rows are still individual evidence"
        );
    }

    /// Evidence rows, the trim and the aggregates are one transaction: a
    /// failure in the last statement leaves none of the first two behind,
    /// so no episode is ever counted without the row it came from.
    #[sqlx::test]
    async fn conflict_evidence_and_stats_commit_together(pool: PgPool) {
        let store = catalog(&pool);
        sqlx::query(
            "ALTER TABLE field_conflict_stats
             ADD CONSTRAINT field_conflict_stats_injected_failure
             CHECK (field <> 'duration')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let err = store
            .record_conflicts(&[FieldConflict {
                field: "duration".to_owned(),
                service: "svc-a".to_owned(),
                observed_type: "VARCHAR".to_owned(),
                expected_type: CanonicalType::BigInt,
                rows_nulled: 3,
                samples: vec!["n/a".to_owned()],
            }])
            .await;
        assert!(err.is_err(), "the injected failure must surface");

        assert!(
            store
                .conflicts_for_field("duration")
                .await
                .unwrap()
                .is_empty(),
            "the evidence row rolled back with the aggregate"
        );
        let stats: i64 =
            sqlx::query_scalar("SELECT count(*) FROM field_conflict_stats WHERE field = $1")
                .bind("duration")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stats, 0, "no partial aggregate row survives");
    }

    #[sqlx::test]
    async fn conflicts_keep_only_the_newest_rows_per_field(pool: PgPool) {
        // A field pinned BIGINT that keeps receiving strings appends a row
        // every compaction tick, forever, while its information content
        // stays constant — so the evidence is a rolling window, trimmed per
        // field (service names are client-chosen too, so a per-service
        // window would only move the unbounded axis).
        let store = catalog(&pool).with_conflict_cap(3);
        let conflict = |service: &str, nulled: u64| FieldConflict {
            field: "duration".to_owned(),
            service: service.to_owned(),
            observed_type: "VARCHAR".to_owned(),
            expected_type: CanonicalType::BigInt,
            rows_nulled: nulled,
            samples: Vec::new(),
        };

        for i in 0..6_u64 {
            store
                .record_conflicts(&[conflict(&format!("svc-{i}"), i)])
                .await
                .unwrap();
        }
        // An untouched field is never trimmed by another field's write.
        store
            .record_conflicts(&[FieldConflict {
                field: "other".to_owned(),
                ..conflict("svc-x", 1)
            }])
            .await
            .unwrap();

        let rows = store.conflicts_for_field("duration").await.unwrap();
        assert_eq!(rows.len(), 3, "the window bounds the evidence per field");
        let kept: Vec<&str> = rows.iter().map(|r| r.service.as_str()).collect();
        assert_eq!(
            kept,
            vec!["svc-5", "svc-4", "svc-3"],
            "the newest rows survive, oldest first out"
        );
        assert_eq!(
            store.conflicts_for_field("other").await.unwrap().len(),
            1,
            "trimming touches only the fields the call wrote"
        );

        // A single over-window batch is trimmed by the same statement that
        // inserted it — the trim must see its own insert.
        let store = catalog(&pool).with_conflict_cap(2);
        store
            .record_conflicts(&[
                FieldConflict {
                    field: "burst".to_owned(),
                    ..conflict("svc-a", 1)
                },
                FieldConflict {
                    field: "burst".to_owned(),
                    ..conflict("svc-b", 2)
                },
                FieldConflict {
                    field: "burst".to_owned(),
                    ..conflict("svc-c", 3)
                },
            ])
            .await
            .unwrap();
        let rows = store.conflicts_for_field("burst").await.unwrap();
        assert_eq!(rows.len(), 2, "one oversized batch is trimmed on arrival");
        let kept: Vec<&str> = rows.iter().map(|r| r.service.as_str()).collect();
        assert_eq!(
            kept,
            vec!["svc-c", "svc-b"],
            "rows sharing an insert timestamp break the tie by id — newest last-written wins"
        );
    }

    // -- catalog read model -------------------------------------------------

    /// Age a `(field, service)` observation into the past by `days`.
    async fn age_observation(pool: &PgPool, field: &str, service: &str, days: i64) {
        sqlx::query(
            "UPDATE field_services
             SET last_seen = now() - make_interval(days => $3::int)
             WHERE field = $1 AND service = $2",
        )
        .bind(field)
        .bind(service)
        .bind(days)
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test]
    async fn list_fields_aggregates_observations_and_conflicts(pool: PgPool) {
        let store = catalog(&pool);
        store
            .pin_missing(&[
                proposal("duration", CanonicalType::BigInt),
                proposal("path", CanonicalType::Varchar),
            ])
            .await
            .unwrap();
        store
            .touch_services("svc-a", &["duration".to_owned()], 3)
            .await
            .unwrap();
        store
            .touch_services("svc-b", &["duration".to_owned()], 2)
            .await
            .unwrap();
        store
            .record_conflicts(&[FieldConflict {
                field: "duration".to_owned(),
                service: "svc-b".to_owned(),
                observed_type: "VARCHAR".to_owned(),
                expected_type: CanonicalType::BigInt,
                rows_nulled: 1,
                samples: Vec::new(),
            }])
            .await
            .unwrap();

        let (rows, truncated) = store
            .list_fields(&trawl_server::store::FieldListFilter::default())
            .await
            .unwrap();
        assert!(!truncated);

        let duration = rows.iter().find(|r| r.field == "duration").unwrap();
        assert_eq!(duration.duckdb_type, "BIGINT");
        assert_eq!(duration.service_count, 2);
        assert_eq!(duration.row_count, 5, "cumulative rows across services");
        assert_eq!(duration.conflict_count, 1);
        assert_eq!(duration.rows_nulled, 1);
        assert!(duration.first_seen.is_some());
        assert!(duration.last_seen.is_some());
        assert_eq!(duration.pinned_from.as_deref(), Some("svc-a"));

        // A pinned-but-never-observed field carries zeroed aggregates.
        let path = rows.iter().find(|r| r.field == "path").unwrap();
        assert_eq!(path.service_count, 0);
        assert_eq!(path.row_count, 0);
        assert_eq!(path.conflict_count, 0);
        assert!(path.first_seen.is_none());
        assert!(path.last_seen.is_none());

        // The envelope seed is part of the listing (it IS pinned).
        assert!(rows.iter().any(|r| r.field == "_time"));
        assert_eq!(rows.len(), ENVELOPE_TYPES.len() + 2);
    }

    /// `/api/v1/schema` wants names and types only. It must not pay for the
    /// conflict evidence — the columns-only listing reads zeroes and skips
    /// the `field_conflicts` query entirely.
    #[sqlx::test]
    async fn list_fields_without_conflicts_reads_zeroes(pool: PgPool) {
        let store = catalog(&pool);
        store
            .pin_missing(&[proposal("duration", CanonicalType::BigInt)])
            .await
            .unwrap();
        store
            .touch_services("svc-a", &["duration".to_owned()], 3)
            .await
            .unwrap();
        store
            .record_conflicts(&[FieldConflict {
                field: "duration".to_owned(),
                service: "svc-a".to_owned(),
                observed_type: "VARCHAR".to_owned(),
                expected_type: CanonicalType::BigInt,
                rows_nulled: 7,
                samples: Vec::new(),
            }])
            .await
            .unwrap();

        let (rows, _) = store
            .list_fields(&trawl_server::store::FieldListFilter {
                with_conflicts: false,
                ..Default::default()
            })
            .await
            .unwrap();
        let duration = rows.iter().find(|r| r.field == "duration").unwrap();
        assert_eq!(duration.duckdb_type, "BIGINT");
        assert_eq!(duration.row_count, 3, "observation aggregates still land");
        assert_eq!(duration.conflict_count, 0, "evidence not requested");
        assert_eq!(duration.rows_nulled, 0, "evidence not requested");

        // The same filter with the evidence sees it, proving the zeroes
        // above are the skip, not a missing conflict row.
        let (rows, _) = store
            .list_fields(&trawl_server::store::FieldListFilter::default())
            .await
            .unwrap();
        let duration = rows.iter().find(|r| r.field == "duration").unwrap();
        assert_eq!(duration.conflict_count, 1);
        assert_eq!(duration.rows_nulled, 7);
    }

    /// The evidence query is keyed on the page the pin listing returned, so
    /// a conflict on a field the page truncated away is never read.
    #[sqlx::test]
    async fn list_fields_conflict_evidence_is_page_scoped(pool: PgPool) {
        let store = catalog(&pool);
        store
            .pin_missing(&[
                proposal("aaa", CanonicalType::BigInt),
                proposal("zzz", CanonicalType::BigInt),
            ])
            .await
            .unwrap();
        store
            .touch_services("svc-a", &["aaa".to_owned(), "zzz".to_owned()], 1)
            .await
            .unwrap();
        store
            .record_conflicts(&[FieldConflict {
                field: "zzz".to_owned(),
                service: "svc-a".to_owned(),
                observed_type: "VARCHAR".to_owned(),
                expected_type: CanonicalType::BigInt,
                rows_nulled: 4,
                samples: Vec::new(),
            }])
            .await
            .unwrap();

        // Scoped to svc-a the candidates are exactly `aaa` and `zzz`, so a
        // one-row page holds `aaa` and cannot contain `zzz`.
        let scoped = |limit: i64| trawl_server::store::FieldListFilter {
            service: Some("svc-a".to_owned()),
            limit,
            ..Default::default()
        };
        let (rows, truncated) = store.list_fields(&scoped(1)).await.unwrap();
        assert!(truncated);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].field, "aaa");
        assert_eq!(rows[0].conflict_count, 0);

        let (rows, _) = store.list_fields(&scoped(10)).await.unwrap();
        let zzz = rows.iter().find(|r| r.field == "zzz").unwrap();
        assert_eq!(zzz.conflict_count, 1, "the full page still sees it");
        assert_eq!(zzz.rows_nulled, 4);
    }

    #[sqlx::test]
    async fn list_fields_service_filter_via_observations(pool: PgPool) {
        let store = catalog(&pool);
        store
            .pin_missing(&[
                proposal("duration", CanonicalType::BigInt),
                proposal("path", CanonicalType::Varchar),
            ])
            .await
            .unwrap();
        store
            .touch_services("nginx", &["duration".to_owned()], 1)
            .await
            .unwrap();
        store
            .touch_services("postgres", &["path".to_owned()], 1)
            .await
            .unwrap();

        let (rows, _) = store
            .list_fields(&trawl_server::store::FieldListFilter {
                service: Some("nginx".to_owned()),
                ..Default::default()
            })
            .await
            .unwrap();
        let names: Vec<&str> = rows.iter().map(|r| r.field.as_str()).collect();
        assert_eq!(
            names,
            vec!["duration"],
            "only fields observed for the service are listed"
        );
    }

    #[sqlx::test]
    async fn list_fields_service_filter_scopes_the_aggregates(pool: PgPool) {
        let store = catalog(&pool);
        store
            .pin_missing(&[proposal("shared", CanonicalType::BigInt)])
            .await
            .unwrap();
        store
            .touch_services("svc-a", &["shared".to_owned()], 1)
            .await
            .unwrap();
        store
            .touch_services("svc-b", &["shared".to_owned()], 3)
            .await
            .unwrap();

        let (rows, _) = store
            .list_fields(&trawl_server::store::FieldListFilter {
                service: Some("svc-a".to_owned()),
                ..Default::default()
            })
            .await
            .unwrap();
        let shared = rows.iter().find(|r| r.field == "shared").unwrap();
        assert_eq!(shared.service_count, 1, "svc-a's own service count");
        assert_eq!(shared.row_count, 1, "svc-b's 3 rows must not leak in");

        // The window reads svc-a's last_seen, not the global max: svc-b is
        // still sending, but that cannot keep the field alive for svc-a.
        age_observation(&pool, "shared", "svc-a", 100).await;
        let since = chrono::Utc::now() - chrono::Duration::days(90);
        let (rows, _) = store
            .list_fields(&trawl_server::store::FieldListFilter {
                service: Some("svc-a".to_owned()),
                since: Some(since),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            !rows.iter().any(|r| r.field == "shared"),
            "aged out for svc-a even though svc-b still observes it"
        );

        // svc-b, and the unscoped listing, still see it.
        let (rows, _) = store
            .list_fields(&trawl_server::store::FieldListFilter {
                service: Some("svc-b".to_owned()),
                since: Some(since),
                ..Default::default()
            })
            .await
            .unwrap();
        let shared = rows.iter().find(|r| r.field == "shared").unwrap();
        assert_eq!(shared.row_count, 3);
    }

    #[sqlx::test]
    async fn list_fields_windows_on_last_seen_keeping_never_observed(pool: PgPool) {
        let store = catalog(&pool);
        store
            .pin_missing(&[proposal("duration", CanonicalType::BigInt)])
            .await
            .unwrap();
        store
            .touch_services("svc-a", &["duration".to_owned()], 1)
            .await
            .unwrap();
        age_observation(&pool, "duration", "svc-a", 100).await;

        let since = chrono::Utc::now() - chrono::Duration::days(90);
        let (rows, _) = store
            .list_fields(&trawl_server::store::FieldListFilter {
                since: Some(since),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            !rows.iter().any(|r| r.field == "duration"),
            "an aged-out field is windowed away"
        );
        assert!(
            rows.iter().any(|r| r.field == "_time"),
            "a never-observed pin (the envelope seed) is ALWAYS shown — \
             there is nothing to age out"
        );

        // Lifting the window (since: None) restores the aged field.
        let (rows, _) = store
            .list_fields(&trawl_server::store::FieldListFilter::default())
            .await
            .unwrap();
        assert!(rows.iter().any(|r| r.field == "duration"));
    }

    #[sqlx::test]
    async fn list_fields_truncates_at_limit(pool: PgPool) {
        let store = catalog(&pool);
        let (rows, truncated) = store
            .list_fields(&trawl_server::store::FieldListFilter {
                limit: 3,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert!(truncated, "more pins exist than the limit returned");

        let (rows, truncated) = store
            .list_fields(&trawl_server::store::FieldListFilter::default())
            .await
            .unwrap();
        assert_eq!(rows.len(), ENVELOPE_TYPES.len());
        assert!(!truncated);
    }

    #[sqlx::test]
    async fn pin_stats_reports_fill_against_cap(pool: PgPool) {
        let store = catalog(&pool);
        let (pinned, cap) = store.pin_stats().await.unwrap();
        assert_eq!(pinned, i64::try_from(ENVELOPE_TYPES.len()).unwrap());
        assert_eq!(cap, trawl_server::store::MAX_PINNED_FIELDS);

        let store = catalog(&pool).with_pin_cap(42);
        let (_, cap) = store.pin_stats().await.unwrap();
        assert_eq!(
            cap, 42,
            "the overridden cap is what fill is measured against"
        );
    }

    #[sqlx::test]
    async fn recent_conflicts_filters_orders_and_windows(pool: PgPool) {
        let store = catalog(&pool);
        let mk = |field: &str, service: &str, nulled: u64| FieldConflict {
            field: field.to_owned(),
            service: service.to_owned(),
            observed_type: "VARCHAR".to_owned(),
            expected_type: CanonicalType::BigInt,
            rows_nulled: nulled,
            samples: Vec::new(),
        };
        store
            .record_conflicts(&[mk("duration", "svc-a", 1)])
            .await
            .unwrap();
        store
            .record_conflicts(&[mk("duration", "svc-b", 2)])
            .await
            .unwrap();
        store
            .record_conflicts(&[mk("path", "svc-a", 3)])
            .await
            .unwrap();

        // Unfiltered: newest first, across fields.
        let (rows, truncated) = store.recent_conflicts(None, None, None, 100).await.unwrap();
        assert_eq!(rows.len(), 3);
        assert!(!truncated);
        assert_eq!(rows[0].field, "path");
        assert_eq!(rows[2].service, "svc-a");
        assert_eq!(rows[2].field, "duration");

        // Field filter.
        let (rows, _) = store
            .recent_conflicts(Some("duration"), None, None, 100)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.field == "duration"));

        // Service filter.
        let (rows, _) = store
            .recent_conflicts(None, Some("svc-b"), None, 100)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].service, "svc-b");

        // Since filter: age one row into the past, then window it away.
        sqlx::query(
            "UPDATE field_conflicts SET at = now() - interval '10 days'
             WHERE field = 'path'",
        )
        .execute(&pool)
        .await
        .unwrap();
        let since = chrono::Utc::now() - chrono::Duration::days(7);
        let (rows, _) = store
            .recent_conflicts(None, None, Some(since), 100)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2, "the aged conflict is outside the window");
        assert!(rows.iter().all(|r| r.field == "duration"));

        // Limit + truncated.
        let (rows, truncated) = store.recent_conflicts(None, None, None, 2).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(truncated);
    }

    #[sqlx::test]
    async fn catalog_id_is_stable_and_conformance_flips_once(pool: PgPool) {
        let store = catalog(&pool);
        let id1 = store.catalog_id().await.unwrap();
        let id2 = store.catalog_id().await.unwrap();
        assert_eq!(id1, id2, "catalog_id is a stable identity");
        assert!(!id1.is_empty());

        assert!(
            !store.is_conformed().await.unwrap(),
            "a fresh catalog has not run the boot conformance pass"
        );
        store.mark_conformed().await.unwrap();
        assert!(store.is_conformed().await.unwrap());
        assert_eq!(
            store.catalog_id().await.unwrap(),
            id1,
            "conformance marking must not rotate the identity"
        );
    }

    /// Pin gc's candidate set and the schema listing's window are two
    /// readings of one fact, `field_services.last_seen`, so for a pin that
    /// has ever been observed they must partition it: alive in the listing,
    /// or a gc candidate, never both and never neither. One cutoff drives
    /// both reads, so a drift in either rule shows up here.
    #[sqlx::test]
    async fn gc_candidates_are_the_complement_of_the_schema_window(pool: PgPool) {
        let store = catalog(&pool);
        store
            .pin_missing(&[
                proposal("fresh", CanonicalType::BigInt),
                proposal("stale", CanonicalType::Varchar),
                proposal("edge", CanonicalType::Double),
                proposal("multi", CanonicalType::Varchar),
            ])
            .await
            .unwrap();

        for field in ["fresh", "stale", "edge", "multi"] {
            store
                .touch_services("svc-a", &[field.to_owned()], 1)
                .await
                .unwrap();
        }
        // `multi` is stale for one sender and current for another: the
        // newest observation across services is what decides it.
        store
            .touch_services("svc-b", &["multi".to_owned()], 1)
            .await
            .unwrap();

        age_observation(&pool, "stale", "svc-a", 60).await;
        age_observation(&pool, "edge", "svc-a", 30).await;
        age_observation(&pool, "multi", "svc-a", 60).await;

        // Between `edge` (30 days back) and `stale` (60), so `edge` is
        // alive and `stale` is not.
        let cutoff = chrono::Utc::now() - chrono::Duration::days(45);

        let candidates = store.pins_unobserved_since(cutoff).await.unwrap();
        let candidate_names: Vec<&str> = candidates.iter().map(|c| c.field.as_str()).collect();
        let (listed, _) = store
            .list_fields(&trawl_server::store::FieldListFilter {
                since: Some(cutoff),
                ..Default::default()
            })
            .await
            .unwrap();
        let listed_names: Vec<&str> = listed.iter().map(|r| r.field.as_str()).collect();

        for field in ["fresh", "stale", "edge", "multi"] {
            let is_candidate = candidate_names.contains(&field);
            let is_listed = listed_names.contains(&field);
            assert!(
                is_candidate != is_listed,
                "{field}: candidate={is_candidate} listed={is_listed} — an \
                 observed pin belongs to exactly one side of the cutoff"
            );
        }
        assert!(candidate_names.contains(&"stale"));
        assert!(!candidate_names.contains(&"fresh"));
        assert!(
            !candidate_names.contains(&"multi"),
            "svc-b still writes it, so the newest observation keeps it alive"
        );

        // The row carries what the report and the audit event need.
        let stale = candidates.iter().find(|c| c.field == "stale").unwrap();
        assert_eq!(stale.duckdb_type, "VARCHAR");
        assert_eq!(stale.services, 1);
        assert!(stale.last_seen.is_some_and(|seen| seen < cutoff));
    }

    /// The one place gc and the listing disagree on purpose: a pin nothing
    /// ever observed is always shown by `/schema` (there is no `last_seen`
    /// to age out) and is a gc candidate (a `curl` typo pinned once and
    /// never written is exactly the slot gc reclaims). Asserted so a later
    /// "make them agree" cleanup has to argue with a test.
    #[sqlx::test]
    async fn never_observed_pin_diverges_from_the_listing_rule(pool: PgPool) {
        let store = catalog(&pool);
        store
            .pin_missing(&[proposal("typoed_feild", CanonicalType::Varchar)])
            .await
            .unwrap();

        let cutoff = chrono::Utc::now() - chrono::Duration::days(30);

        let candidates = store.pins_unobserved_since(cutoff).await.unwrap();
        let typoed = candidates
            .iter()
            .find(|c| c.field == "typoed_feild")
            .expect("a never-observed pin is a candidate");
        assert_eq!(typoed.last_seen, None);
        assert_eq!(typoed.services, 0);

        let (listed, _) = store
            .list_fields(&trawl_server::store::FieldListFilter {
                since: Some(cutoff),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            listed.iter().any(|r| r.field == "typoed_feild"),
            "the listing shows a never-observed pin at the same cutoff"
        );

        // The envelope seed is never observed either, so it comes back here
        // too — the store reads, and `catalog::gc::candidacy` is what
        // refuses to reclaim a contract field.
        assert!(candidates.iter().any(|c| c.field == "_severity"));
    }

    /// The epoch is a cutoff postgres will actually take.
    ///
    /// `catalog::gc::cutoff_for` clamps an unsubtractable window there, and
    /// the point of the clamp is that the query still runs: chrono's
    /// minimum is outside `timestamptz`, so binding it fails at the driver
    /// and a huge `--older-than` would 503 instead of matching almost
    /// nothing. At the epoch every observed pin is alive and only the
    /// never-observed ones come back.
    #[sqlx::test]
    async fn the_epoch_cutoff_binds_and_leaves_only_never_observed_pins(pool: PgPool) {
        let store = catalog(&pool);
        store
            .pin_missing(&[
                proposal("observed", CanonicalType::BigInt),
                proposal("never_observed", CanonicalType::Varchar),
            ])
            .await
            .unwrap();
        store
            .touch_services("svc-a", &["observed".to_owned()], 1)
            .await
            .unwrap();

        let candidates = store
            .pins_unobserved_since(chrono::DateTime::UNIX_EPOCH)
            .await
            .expect("the epoch is inside timestamptz, so the bind succeeds");
        let names: Vec<&str> = candidates.iter().map(|c| c.field.as_str()).collect();
        assert!(names.contains(&"never_observed"));
        assert!(
            !names.contains(&"observed"),
            "no observation predates 1970, so an observed pin is alive at the epoch"
        );
    }

    #[sqlx::test]
    async fn delete_pins_refuses_a_contract_typed_name(pool: PgPool) {
        let store = catalog(&pool);
        store
            .pin_missing(&[proposal("duration", CanonicalType::BigInt)])
            .await
            .unwrap();

        for contract in ["_severity", "service", "_time", "message"] {
            let err = store
                .delete_pins(&[contract.to_owned(), "duration".to_owned()])
                .await
                .expect_err("a contract field is not reclaimable");
            assert!(
                matches!(&err, trawl_server::store::StoreError::Validation(msg)
                    if msg.contains(contract)),
                "{contract}: unexpected error {err:?}"
            );
        }

        // Nothing was deleted: the refusal precedes the transaction, so the
        // ordinary field named beside the contract one survives too.
        let pins = store.load_pins().await.unwrap();
        assert!(pins.iter().any(|(f, _)| f == "duration"));
        assert!(pins.iter().any(|(f, _)| f == "_severity"));
    }

    /// Gc's whole safety argument is that being wrong costs a re-pin: the
    /// purge leaves no trace in any of the four tables, so the field comes
    /// back through the ordinary ingest path as if it were new.
    #[sqlx::test]
    async fn delete_pins_leaves_no_residue_and_a_clean_repin_follows(pool: PgPool) {
        let store = catalog(&pool);
        store
            .pin_missing(&[
                proposal("dead", CanonicalType::BigInt),
                proposal("keep", CanonicalType::Varchar),
            ])
            .await
            .unwrap();
        for field in ["dead", "keep"] {
            store
                .touch_services("svc-a", &[field.to_owned()], 5)
                .await
                .unwrap();
            store
                .record_conflicts(&[FieldConflict {
                    field: field.to_owned(),
                    service: "svc-a".to_owned(),
                    observed_type: "VARCHAR".to_owned(),
                    expected_type: CanonicalType::BigInt,
                    rows_nulled: 2,
                    samples: vec!["accepted".to_owned()],
                }])
                .await
                .unwrap();
        }

        let purged = store.delete_pins(&["dead".to_owned()]).await.unwrap();
        assert_eq!(
            purged
                .deleted
                .iter()
                .map(|p| p.field.as_str())
                .collect::<Vec<_>>(),
            vec!["dead"]
        );

        for (table, sql) in [
            (
                "field_types",
                "SELECT count(*)::bigint FROM field_types WHERE field = $1",
            ),
            (
                "field_services",
                "SELECT count(*)::bigint FROM field_services WHERE field = $1",
            ),
            (
                "field_conflicts",
                "SELECT count(*)::bigint FROM field_conflicts WHERE field = $1",
            ),
            (
                "field_conflict_stats",
                "SELECT count(*)::bigint FROM field_conflict_stats WHERE field = $1",
            ),
        ] {
            let purged: i64 = sqlx::query_scalar(sql)
                .bind("dead")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(purged, 0, "{table} still holds rows for the purged field");
            let kept: i64 = sqlx::query_scalar(sql)
                .bind("keep")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert!(kept > 0, "{table} lost the neighbouring field's rows");
        }

        // Re-arrival through the normal path: a fresh pin, its own type,
        // and a first observation. Nothing carried over from the old life.
        let pins = store
            .pin_missing(&[proposal("dead", CanonicalType::Varchar)])
            .await
            .unwrap();
        assert_eq!(
            pins.get("dead"),
            Some(&CanonicalType::Varchar),
            "the slot is genuinely free — the old BIGINT pin did not win"
        );
        store
            .touch_services("svc-b", &["dead".to_owned()], 1)
            .await
            .unwrap();

        let (rows, _) = store
            .list_fields(&trawl_server::store::FieldListFilter::default())
            .await
            .unwrap();
        let reborn = rows.iter().find(|r| r.field == "dead").unwrap();
        assert_eq!(reborn.duckdb_type, "VARCHAR");
        assert_eq!(reborn.service_count, 1, "svc-a's history did not survive");
        assert_eq!(reborn.row_count, 1);
        assert_eq!(reborn.conflict_count, 0, "old evidence did not survive");
    }

    /// The purge hands back everything the caller needs to finish the
    /// reclaim, all read inside the one transaction: the names
    /// `field_types` actually gave up, and the pins remaining for the fill
    /// gauges. The caller's next act is an eviction that must not be
    /// skipped, so a post-commit SELECT for the gauge would be a fallible
    /// step in exactly the wrong place.
    ///
    /// The returned rows are the DELETE's own RETURNING set, never the
    /// request: a candidate whose row went away underneath the run must not
    /// show up in an audit event claiming this run deleted it. Their
    /// metadata is the transaction's own read, never the caller's earlier
    /// snapshot — a repin can retype a row between the two, and the audit
    /// record is the only surviving account of what was deleted.
    #[sqlx::test]
    async fn delete_pins_returns_the_deleted_metadata_and_the_fill_count(pool: PgPool) {
        let store = catalog(&pool);
        store
            .pin_missing(&[
                proposal("dead", CanonicalType::BigInt),
                proposal("keep", CanonicalType::Varchar),
            ])
            .await
            .unwrap();
        for service in ["svc-a", "svc-b"] {
            store
                .touch_services(service, &["dead".to_owned()], 3)
                .await
                .unwrap();
        }
        let before: i64 = sqlx::query_scalar("SELECT count(*)::bigint FROM field_types")
            .fetch_one(&pool)
            .await
            .unwrap();

        // The staleness the caller cannot avoid: it read BIGINT, and a
        // repin cutover retyped the row before the purge ran.
        sqlx::query("UPDATE field_types SET duckdb_type = 'VARCHAR' WHERE field = 'dead'")
            .execute(&pool)
            .await
            .unwrap();

        let purged = store
            .delete_pins(&["dead".to_owned(), "never_pinned".to_owned()])
            .await
            .unwrap();

        assert_eq!(
            purged
                .deleted
                .iter()
                .map(|p| p.field.as_str())
                .collect::<Vec<_>>(),
            vec!["dead"],
            "a name the catalog never held is not a name this run deleted"
        );
        let pin = &purged.deleted[0];
        assert_eq!(
            pin.duckdb_type, "VARCHAR",
            "the metadata is the row the transaction deleted, not the \
             BIGINT a pre-purge snapshot would have carried"
        );
        assert_eq!(pin.pinned_from.as_deref(), Some("svc-a"));
        assert_eq!(
            pin.services, 2,
            "both observations were counted before they were deleted"
        );
        assert!(
            pin.last_seen.is_some(),
            "the newest observation is captured while field_services still \
             holds it"
        );
        assert!(pin.pinned_at <= chrono::Utc::now());
        let after: i64 = sqlx::query_scalar("SELECT count(*)::bigint FROM field_types")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(purged.pinned_now, before - 1);
        assert_eq!(purged.pinned_now, after, "the count is the committed one");
    }

    /// The purge bounds its pre-commit phase twice over. A row lock
    /// somebody else holds costs it five seconds and an error, never an
    /// unbounded wait, while the corpus gate and every compaction batch
    /// queued behind it wait on the answer.
    ///
    /// Two bounds cover this, and either refusal is correct: postgres'
    /// `lock_timeout` at five seconds, and the caller's ten-second
    /// `PURGE_PREPARE_BOUND` for the case postgres cannot see, a connection
    /// that stops answering while the backend sits idle. What matters is
    /// that the wait ends and the transaction rolls back; which bound
    /// noticed is not the contract.
    ///
    /// The purge takes the row lock at `DELETE FROM field_types`, so this
    /// exercises the `lock_timeout` half; the `statement_timeout` half is
    /// what bounds the advisory lock a step earlier, which no test can hold
    /// from outside (`CATALOG_LIFECYCLE_LOCK_KEY` is crate-private).
    #[sqlx::test]
    async fn delete_pins_gives_up_on_a_held_row_lock_instead_of_waiting(pool: PgPool) {
        let store = catalog(&pool);
        store
            .pin_missing(&[proposal("dead", CanonicalType::BigInt)])
            .await
            .unwrap();

        let mut blocker = pool.begin().await.unwrap();
        sqlx::query("SELECT field FROM field_types WHERE field = 'dead' FOR UPDATE")
            .fetch_all(&mut *blocker)
            .await
            .unwrap();

        let started = std::time::Instant::now();
        let err = store
            .delete_pins(&["dead".to_owned()])
            .await
            .expect_err("a purge that cannot take its row lock must fail, not hang");
        let waited = started.elapsed();
        blocker.rollback().await.unwrap();

        assert!(
            waited < std::time::Duration::from_secs(30),
            "the purge waited {waited:?} on a held row lock; the db-side bound \
             is not in force, and a wrapped timeout is not an option here"
        );
        assert!(
            matches!(
                err,
                trawl_server::store::StoreError::Unavailable(_)
                    | trawl_server::store::StoreError::PurgePrepareTimeout
            ),
            "unexpected error {err:?}"
        );
        assert!(
            store
                .load_pins()
                .await
                .unwrap()
                .iter()
                .any(|(f, _)| f == "dead"),
            "a bounded-out purge rolls back and deletes nothing"
        );
    }

    /// The purge's own half of the pin-gc race: the running-row check
    /// lives INSIDE the transaction, under the catalog lifecycle lock, so
    /// a claim that landed after gc's gated courtesy look still stops the
    /// delete. Nothing is deleted, and the slot is free again once the job
    /// is terminal.
    #[sqlx::test]
    async fn delete_pins_refuses_in_the_transaction_while_a_repin_runs(pool: PgPool) {
        let store = catalog(&pool);
        store
            .pin_missing(&[proposal("dead", CanonicalType::BigInt)])
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO repin_jobs (field, from_type, to_type, dry_run, status, requested_by)
             VALUES ('host', 'VARCHAR', 'BIGINT', FALSE, 'running', 'ops')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let err = store
            .delete_pins(&["dead".to_owned()])
            .await
            .expect_err("a running repin job refuses the purge");
        assert!(
            matches!(err, trawl_server::store::StoreError::RepinAlreadyRunning),
            "unexpected error {err:?}"
        );
        assert!(
            store
                .load_pins()
                .await
                .unwrap()
                .iter()
                .any(|(f, _)| f == "dead"),
            "a refused purge deletes nothing"
        );

        sqlx::query("UPDATE repin_jobs SET status = 'failed', finished_at = now()")
            .execute(&pool)
            .await
            .unwrap();
        store
            .delete_pins(&["dead".to_owned()])
            .await
            .expect("a terminal job frees the purge");
    }

    /// The interleaving the lock closes, run for real: a purge and a claim
    /// for the same field, concurrently. Whichever transaction takes
    /// `CATALOG_LIFECYCLE_LOCK_KEY` first, exactly one succeeds — and the
    /// corrupt outcome (the pin deleted AND a job claimed against it, whose
    /// cutover would then restore the pin in memory only) is unreachable.
    #[sqlx::test]
    async fn a_concurrent_purge_and_claim_never_both_win(pool: PgPool) {
        let store = catalog(&pool);
        let repin = trawl_server::store::RepinStore::new(pool.clone());
        store
            .pin_missing(&[proposal("dur", CanonicalType::BigInt)])
            .await
            .unwrap();

        let purger = store.clone();
        let purge = tokio::spawn(async move { purger.delete_pins(&["dur".to_owned()]).await });
        let claim = tokio::spawn(async move {
            repin
                .claim(trawl_server::store::RepinClaim {
                    field: "dur",
                    from_type: CanonicalType::BigInt,
                    to_type: CanonicalType::Varchar,
                    dialect: None,
                    dry_run: false,
                    force: false,
                    requested_by: Some("ops"),
                })
                .await
        });
        let purged = purge.await.expect("purge task");
        let claimed = claim.await.expect("claim task");

        let pin_gone = !store
            .load_pins()
            .await
            .unwrap()
            .iter()
            .any(|(f, _)| f == "dur");
        assert!(
            !(pin_gone && claimed.is_ok()),
            "the corrupt outcome: pin deleted and a claim taken against it \
             (purge={purged:?}, claim={claimed:?})"
        );
        assert_eq!(
            usize::from(purged.is_ok()) + usize::from(claimed.is_ok()),
            1,
            "exactly one of the two commits: purge={purged:?}, claim={claimed:?}"
        );
        assert_eq!(
            pin_gone,
            purged.is_ok(),
            "the pin is gone exactly when the purge won"
        );
    }

    /// A repin's history is what an operator did to the corpus, and it
    /// stays true after the field is gone.
    #[sqlx::test]
    async fn delete_pins_never_touches_repin_history(pool: PgPool) {
        let store = catalog(&pool);
        store
            .pin_missing(&[proposal("dead", CanonicalType::BigInt)])
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO repin_jobs (field, from_type, to_type, dry_run, status, requested_by)
             VALUES ('dead', 'BIGINT', 'VARCHAR', FALSE, 'succeeded', 'ops')",
        )
        .execute(&pool)
        .await
        .unwrap();

        store.delete_pins(&["dead".to_owned()]).await.unwrap();

        let jobs: i64 =
            sqlx::query_scalar("SELECT count(*)::bigint FROM repin_jobs WHERE field = 'dead'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(jobs, 1);
    }
}

// ---------------------------------------------------------------------------
// repin jobs
// ---------------------------------------------------------------------------

mod repin_store {
    use sqlx::PgPool;
    use trawl_core::schema::CanonicalType;
    use trawl_server::store::{
        CatalogStore, RepinClaim, RepinJobStatus, RepinPlan, RepinStore, StoreError,
    };

    fn store(pool: &PgPool) -> RepinStore {
        RepinStore::new(pool.clone())
    }

    /// Seed the `field_types` row a claim revalidates against.
    ///
    /// `claim` refuses a field whose pin is not the one the caller prepared
    /// against, which is what closes the pin-gc race: the engine reads the
    /// pin from its cache, so the claim transaction proves it is still
    /// there. Every claim in these tests therefore needs the pin it names.
    async fn pin(pool: &PgPool, field: &str, ty: CanonicalType) {
        sqlx::query(
            "INSERT INTO field_types (field, duckdb_type, pinned_from)
             VALUES ($1, $2, '_test')
             ON CONFLICT (field) DO UPDATE SET duckdb_type = EXCLUDED.duckdb_type",
        )
        .bind(field)
        .bind(ty.as_catalog())
        .execute(pool)
        .await
        .expect("seed a pin");
    }

    /// One repin at a time, enforced by the partial unique index — the
    /// second claim maps the named violation, never a raw pg error.
    #[sqlx::test]
    async fn second_claim_is_repin_already_running(pool: PgPool) {
        let s = store(&pool);
        pin(&pool, "status", CanonicalType::BigInt).await;
        pin(&pool, "dur", CanonicalType::Varchar).await;
        let id = s
            .claim(RepinClaim {
                field: "status",
                from_type: CanonicalType::BigInt,
                to_type: CanonicalType::Varchar,
                dialect: None,
                dry_run: false,
                force: false,
                requested_by: Some("key-1"),
            })
            .await
            .unwrap();
        assert!(id > 0);

        let err = s
            .claim(RepinClaim {
                field: "dur",
                from_type: CanonicalType::Varchar,
                to_type: CanonicalType::BigInt,
                dialect: None,
                dry_run: false,
                force: false,
                requested_by: None,
            })
            .await
            .expect_err("a second running job must refuse");
        assert!(matches!(err, StoreError::RepinAlreadyRunning));

        // A terminal job frees the slot.
        assert!(
            s.finish_if_running(id, RepinJobStatus::Failed, Some("test"))
                .await
                .unwrap()
        );
        s.claim(RepinClaim {
            field: "dur",
            from_type: CanonicalType::Varchar,
            to_type: CanonicalType::BigInt,
            dialect: None,
            dry_run: false,
            force: false,
            requested_by: None,
        })
        .await
        .expect("a terminal job frees the one-running slot");
    }

    /// Two concurrent claims: exactly one wins, and the loser sees the
    /// domain error rather than a raw pg violation.
    #[sqlx::test]
    async fn concurrent_claims_admit_exactly_one(pool: PgPool) {
        pin(&pool, "status", CanonicalType::BigInt).await;
        let s1 = store(&pool);
        let s2 = store(&pool);
        let (a, b) = tokio::join!(
            s1.claim(RepinClaim {
                field: "status",
                from_type: CanonicalType::BigInt,
                to_type: CanonicalType::Varchar,
                dialect: None,
                dry_run: false,
                force: false,
                requested_by: None,
            }),
            s2.claim(RepinClaim {
                field: "status",
                from_type: CanonicalType::BigInt,
                to_type: CanonicalType::Varchar,
                dialect: None,
                dry_run: false,
                force: false,
                requested_by: None,
            }),
        );
        let wins = [&a, &b].iter().filter(|r| r.is_ok()).count();
        assert_eq!(wins, 1, "exactly one claim may win: {a:?} / {b:?}");
        let loser = if a.is_err() { a } else { b };
        assert!(matches!(loser, Err(StoreError::RepinAlreadyRunning)));
    }

    /// The cutover flip is one transaction: the pin's stored type and the
    /// job's completion move together, and re-running it (crash recovery's
    /// idempotent redo) changes nothing.
    #[sqlx::test]
    async fn finish_cutover_flips_pin_and_job_transactionally_and_idempotently(pool: PgPool) {
        let s = store(&pool);
        let catalog = CatalogStore::new(pool.clone());
        // `host` is seeded VARCHAR by migration 0002; the flip retypes it.
        let id = s
            .claim(RepinClaim {
                field: "host",
                from_type: CanonicalType::Varchar,
                to_type: CanonicalType::BigInt,
                dialect: None,
                dry_run: false,
                force: false,
                requested_by: None,
            })
            .await
            .unwrap();

        s.finish_cutover(id, "host", CanonicalType::BigInt)
            .await
            .unwrap();

        let pins: std::collections::HashMap<_, _> =
            catalog.load_pins().await.unwrap().into_iter().collect();
        assert_eq!(pins.get("host"), Some(&CanonicalType::BigInt));
        let job = s.get(id).await.unwrap().expect("job row");
        assert_eq!(job.status, RepinJobStatus::Succeeded);
        assert!(job.finished_at.is_some());

        // Idempotent redo (boot recovery replays the flip).
        s.finish_cutover(id, "host", CanonicalType::BigInt)
            .await
            .unwrap();
        let again = s.get(id).await.unwrap().unwrap();
        assert_eq!(again.status, RepinJobStatus::Succeeded);
        assert_eq!(again.finished_at, job.finished_at, "redo must not restamp");
    }

    /// The cutover clears the field's conflict evidence in the same
    /// transaction as the flip: it indicts a pin that no longer exists, and
    /// the analyzer's gate is span-based, so leaving it would badge the
    /// field as degraded forever — the remedy would not clear the sign.
    /// Another field's evidence is untouched.
    #[sqlx::test]
    async fn finish_cutover_clears_the_repinned_field_evidence(pool: PgPool) {
        let s = store(&pool);
        let catalog = CatalogStore::new(pool.clone());
        let conflict = |field: &str| trawl_server::store::FieldConflict {
            field: field.to_owned(),
            service: "svc-a".to_owned(),
            observed_type: "VARCHAR".to_owned(),
            expected_type: CanonicalType::BigInt,
            rows_nulled: 4,
            samples: vec!["n/a".to_owned()],
        };
        catalog
            .record_conflicts(&[conflict("severity"), conflict("message")])
            .await
            .unwrap();
        pin(&pool, "severity", CanonicalType::BigInt).await;

        let id = s
            .claim(RepinClaim {
                field: "severity",
                from_type: CanonicalType::BigInt,
                to_type: CanonicalType::Varchar,
                dialect: None,
                dry_run: false,
                force: false,
                requested_by: None,
            })
            .await
            .unwrap();
        s.finish_cutover(id, "severity", CanonicalType::Varchar)
            .await
            .unwrap();

        assert!(
            catalog
                .conflicts_for_field("severity")
                .await
                .unwrap()
                .is_empty(),
            "the repinned field's detail evidence is gone"
        );
        assert!(
            catalog
                .conflict_aggregates(Some(&["severity".to_owned()]))
                .await
                .unwrap()
                .is_empty(),
            "and so are its durable aggregates"
        );
        assert_eq!(
            catalog
                .conflict_aggregates(Some(&["message".to_owned()]))
                .await
                .unwrap()
                .len(),
            1,
            "another field's evidence survives"
        );
    }

    /// A replay of an already-succeeded cutover must not clear evidence.
    ///
    /// A forced lossy repin records its own conflict evidence, describing
    /// the new pin, after the flip commits. Boot recovery replays
    /// `finish_cutover` whenever the marker outlived the cleanup window, and
    /// an unconditional clear would delete exactly that evidence, which
    /// nothing writes again.
    #[sqlx::test]
    async fn finish_cutover_replay_keeps_evidence_recorded_after_the_flip(pool: PgPool) {
        let s = store(&pool);
        let catalog = CatalogStore::new(pool.clone());
        pin(&pool, "severity", CanonicalType::BigInt).await;
        let id = s
            .claim(RepinClaim {
                field: "severity",
                from_type: CanonicalType::BigInt,
                to_type: CanonicalType::Varchar,
                dialect: None,
                dry_run: false,
                force: true,
                requested_by: None,
            })
            .await
            .unwrap();
        s.finish_cutover(id, "severity", CanonicalType::Varchar)
            .await
            .unwrap();

        // What the forced job's own `record_outcome` writes next.
        catalog
            .record_conflicts(&[trawl_server::store::FieldConflict {
                field: "severity".to_owned(),
                service: "svc-a".to_owned(),
                observed_type: "BIGINT".to_owned(),
                expected_type: CanonicalType::Varchar,
                rows_nulled: 2,
                samples: Vec::new(),
            }])
            .await
            .unwrap();

        s.finish_cutover(id, "severity", CanonicalType::Varchar)
            .await
            .unwrap();

        assert_eq!(
            catalog.conflicts_for_field("severity").await.unwrap().len(),
            1,
            "the replay must not touch evidence written after the flip"
        );
        assert_eq!(
            catalog
                .conflict_aggregates(Some(&["severity".to_owned()]))
                .await
                .unwrap()
                .len(),
            1,
            "nor its aggregates"
        );
    }

    /// Boot reconciliation: an orphaned `running` row (killed process, no
    /// marker) fails; the marker's own job — mid-recovery — is kept.
    #[sqlx::test]
    async fn reconcile_orphans_fails_running_rows_except_the_kept_one(pool: PgPool) {
        pin(&pool, "status", CanonicalType::BigInt).await;
        let s = store(&pool);
        let id = s
            .claim(RepinClaim {
                field: "status",
                from_type: CanonicalType::BigInt,
                to_type: CanonicalType::Varchar,
                dialect: None,
                dry_run: false,
                force: false,
                requested_by: None,
            })
            .await
            .unwrap();

        assert_eq!(s.reconcile_orphans(Some(id)).await.unwrap(), 0);
        assert_eq!(
            s.get(id).await.unwrap().unwrap().status,
            RepinJobStatus::Running,
            "the marker's job survives reconciliation"
        );

        assert_eq!(s.reconcile_orphans(None).await.unwrap(), 1);
        let job = s.get(id).await.unwrap().unwrap();
        assert_eq!(job.status, RepinJobStatus::Failed);
        assert!(job.error.is_some());
    }

    /// Plan + progress land on the row; `latest` prefers the running job
    /// and falls back to the newest terminal one.
    #[sqlx::test]
    async fn plan_progress_and_latest(pool: PgPool) {
        pin(&pool, "status", CanonicalType::BigInt).await;
        let s = store(&pool);
        assert!(s.latest().await.unwrap().is_none());

        let first = s
            .claim(RepinClaim {
                field: "status",
                from_type: CanonicalType::BigInt,
                to_type: CanonicalType::Varchar,
                dialect: None,
                dry_run: true,
                force: false,
                requested_by: None,
            })
            .await
            .unwrap();
        s.record_plan(
            first,
            RepinPlan {
                files_total: 12,
                rows_carrying: 3400,
                projected_nulls: 25,
                resurrectable: 19,
                affected_bytes: 1 << 20,
                ambiguous_numerals: 4,
                unmapped_samples: vec!["gold".to_owned(), "platinum".to_owned()],
                field_last_seen: Some(chrono::Utc::now()),
                field_last_service: Some("nginx".to_owned()),
            },
        )
        .await
        .unwrap();
        assert!(
            s.finish_if_running(first, RepinJobStatus::Succeeded, None)
                .await
                .unwrap()
        );

        let second = s
            .claim(RepinClaim {
                field: "status",
                from_type: CanonicalType::BigInt,
                to_type: CanonicalType::Varchar,
                dialect: None,
                dry_run: false,
                force: true,
                requested_by: Some("key-9"),
            })
            .await
            .unwrap();
        s.record_progress(second, 5, 1200, 2, 7, 3).await.unwrap();

        let latest = s.latest().await.unwrap().expect("a running job");
        assert_eq!(latest.id, second);
        assert_eq!(latest.status, RepinJobStatus::Running);
        assert!(latest.force);
        assert!(!latest.dry_run);
        assert_eq!(latest.requested_by.as_deref(), Some("key-9"));
        assert_eq!(latest.files_done, 5);
        assert_eq!(latest.rows_rewritten, 1200);
        assert_eq!(latest.rows_nulled, 2);
        assert_eq!(latest.rows_resurrected, 7);
        assert_eq!(
            latest.ambiguous_numerals, 3,
            "the shadow's own ambiguity count supersedes the scan's"
        );

        assert!(
            s.finish_if_running(second, RepinJobStatus::Blocked, Some("cutover starved"))
                .await
                .unwrap()
        );
        let latest = s.latest().await.unwrap().expect("newest terminal job");
        assert_eq!(latest.id, second);
        assert_eq!(latest.status, RepinJobStatus::Blocked);
        assert_eq!(latest.error.as_deref(), Some("cutover starved"));

        let dry = s.get(first).await.unwrap().unwrap();
        assert!(dry.dry_run);
        assert_eq!(dry.files_total, 12);
        assert_eq!(dry.rows_carrying, 3400);
        assert_eq!(dry.projected_nulls, 25);
        assert_eq!(dry.resurrectable, 19);
        assert_eq!(dry.affected_bytes, 1 << 20);
        assert_eq!(dry.ambiguous_numerals, 4, "the plan's own count stands");
        assert_eq!(dry.unmapped_samples, vec!["gold", "platinum"]);
        assert!(dry.field_last_seen.is_some(), "liveness rides the plan");
        assert_eq!(dry.field_last_service.as_deref(), Some("nginx"));
        // Nothing asserted a dialect, so the row reports none, never a
        // backfilled `otel`.
        assert_eq!(dry.dialect, None);
    }

    /// The SEVERITY target and its dialect (migration 0012): both type
    /// CHECKs admit the catalog spelling, so a repin away from a severity pin
    /// claims a job whose `from_type` is SEVERITY, and the dialect is stored
    /// exactly for a severity target. The scope CHECK runs both ways, so a
    /// row can never carry an assertion nothing read, nor omit one the
    /// rewrite needed.
    #[sqlx::test]
    async fn a_severity_repin_stores_its_asserted_dialect(pool: PgPool) {
        use trawl_core::severity::Dialect;

        let s = store(&pool);
        pin(&pool, "level", CanonicalType::Varchar).await;
        let claim = |to, dialect, dry_run| RepinClaim {
            field: "level",
            from_type: CanonicalType::Varchar,
            to_type: to,
            dialect,
            dry_run,
            force: false,
            requested_by: None,
        };

        let id = s
            .claim(claim(CanonicalType::Severity, Some(Dialect::Syslog), true))
            .await
            .expect("SEVERITY is a claimable target");
        let job = s.get(id).await.unwrap().unwrap();
        assert_eq!(job.to_type, "SEVERITY");
        assert_eq!(job.dialect.as_deref(), Some("syslog"));
        assert!(
            s.finish_if_running(id, RepinJobStatus::Succeeded, None)
                .await
                .unwrap()
        );

        // And back off the severity pin, which 0012's widened `from_type`
        // CHECK admits. The pin itself moved with the succeeded job.
        pin(&pool, "level", CanonicalType::Severity).await;
        let back = s
            .claim(RepinClaim {
                field: "level",
                from_type: CanonicalType::Severity,
                to_type: CanonicalType::Varchar,
                dialect: None,
                dry_run: true,
                force: false,
                requested_by: None,
            })
            .await
            .expect("a repin away from SEVERITY is claimable");
        let job = s.get(back).await.unwrap().unwrap();
        assert_eq!(job.from_type, "SEVERITY");
        assert_eq!(job.dialect, None);
        assert!(
            s.finish_if_running(back, RepinJobStatus::Succeeded, None)
                .await
                .unwrap()
        );

        // The scope CHECK, both directions: a severity target with no
        // dialect, and a dialect on any other target, are corruption the
        // store refuses rather than stores. Both go through the VARCHAR
        // pin the closure names.
        pin(&pool, "level", CanonicalType::Varchar).await;
        s.claim(claim(CanonicalType::Severity, None, true))
            .await
            .expect_err("a SEVERITY job must carry a dialect");
        s.claim(claim(CanonicalType::BigInt, Some(Dialect::Otel), true))
            .await
            .expect_err("only a SEVERITY job may carry a dialect");
    }

    /// The claim's half of the pin-gc race: the engine reads a field's pin
    /// from the in-process cache, so the claim transaction proves — under
    /// the catalog lifecycle lock — that `field_types` still carries it. A
    /// pin gc purge that got there first leaves the claim refusing with the
    /// engine's own unpinned-field sentence, and no job row behind.
    #[sqlx::test]
    async fn claim_refuses_when_the_from_pin_vanished(pool: PgPool) {
        let s = store(&pool);
        let catalog = CatalogStore::new(pool.clone());
        pin(&pool, "dur", CanonicalType::BigInt).await;
        catalog
            .delete_pins(&["dur".to_owned()])
            .await
            .expect("gc reclaims the slot");

        let taken = |from| RepinClaim {
            field: "dur",
            from_type: from,
            to_type: CanonicalType::Varchar,
            dialect: None,
            dry_run: false,
            force: false,
            requested_by: Some("ops"),
        };
        let err = s
            .claim(taken(CanonicalType::BigInt))
            .await
            .expect_err("a vanished pin refuses the claim");
        assert!(
            matches!(&err, StoreError::RepinPinVanished { field, found, .. }
                if field == "dur" && found.is_none()),
            "unexpected error {err:?}"
        );
        assert!(
            err.to_string().contains("is not a pinned field"),
            "the operator reads the engine's own sentence: {err}"
        );
        let jobs: i64 = sqlx::query_scalar("SELECT count(*)::bigint FROM repin_jobs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(jobs, 0, "a refused claim leaves no job row");

        // A pin that merely CHANGED under the caller refuses too, with its
        // own sentence: retrying against the current pin is the remedy.
        pin(&pool, "dur", CanonicalType::Varchar).await;
        let err = s
            .claim(taken(CanonicalType::BigInt))
            .await
            .expect_err("a changed pin refuses the claim");
        assert!(
            matches!(&err, StoreError::RepinPinVanished { found, .. }
                if found.as_deref() == Some("VARCHAR")),
            "unexpected error {err:?}"
        );

        // Against the pin it actually holds, the same claim goes through.
        s.claim(taken(CanonicalType::Varchar))
            .await
            .expect("the current pin is claimable");
    }

    /// The re-arm switch for boot recovery: a recovered cutover clears
    /// `conformed_at` so the next conformance pass re-proves the corpus.
    #[sqlx::test]
    async fn clear_conformed_rearms_the_boot_pass(pool: PgPool) {
        let catalog = CatalogStore::new(pool.clone());
        catalog.mark_conformed().await.unwrap();
        assert!(catalog.is_conformed().await.unwrap());
        catalog.clear_conformed().await.unwrap();
        assert!(!catalog.is_conformed().await.unwrap());
    }

    /// A claimed job to cancel, with nothing else asserted.
    async fn claim_running(s: &RepinStore) -> i64 {
        s.claim(RepinClaim {
            field: "status",
            from_type: CanonicalType::BigInt,
            to_type: CanonicalType::Varchar,
            dialect: None,
            dry_run: false,
            force: false,
            requested_by: Some("key-1"),
        })
        .await
        .unwrap()
    }

    /// The cancel round trip: the request lands on the running row, the job
    /// terminalizes `cancelled`, and both columns read back.
    #[sqlx::test]
    async fn a_cancelled_job_keeps_its_request_fields(pool: PgPool) {
        let s = store(&pool);
        let id = claim_running(&s).await;

        let job = s
            .record_cancel_request(id, "key-op")
            .await
            .unwrap()
            .expect("a running job accepts the request");
        assert_eq!(job.cancelled_by.as_deref(), Some("key-op"));
        let requested_at = job.cancel_requested_at.expect("both halves are written");
        assert_eq!(
            job.status,
            RepinJobStatus::Running,
            "the request is not the effect"
        );

        assert!(
            s.finish_if_running(
                id,
                RepinJobStatus::Cancelled,
                Some("cancelled by key-op during build; the live corpus was never touched"),
            )
            .await
            .unwrap()
        );

        let job = s.get(id).await.unwrap().unwrap();
        assert_eq!(job.status, RepinJobStatus::Cancelled);
        assert_eq!(job.cancelled_by.as_deref(), Some("key-op"));
        assert_eq!(job.cancel_requested_at, Some(requested_at));
        assert!(job.finished_at.is_some());
    }

    /// First writer wins: a second operator cancelling a job already
    /// cancelling changes neither the actor nor the instant, so the audit
    /// trail names the request that actually took effect. A request against
    /// a terminal job is `None` — it lost the race with the job's own
    /// ladder.
    #[sqlx::test]
    async fn a_second_cancel_request_preserves_the_first(pool: PgPool) {
        let s = store(&pool);
        let id = claim_running(&s).await;

        let first = s
            .record_cancel_request(id, "key-op")
            .await
            .unwrap()
            .unwrap();
        let second = s
            .record_cancel_request(id, "key-other")
            .await
            .unwrap()
            .expect("the job is still running");
        assert_eq!(second.cancelled_by.as_deref(), Some("key-op"));
        assert_eq!(second.cancel_requested_at, first.cancel_requested_at);

        assert!(
            s.finish_if_running(id, RepinJobStatus::Cancelled, Some("cancelled"))
                .await
                .unwrap()
        );
        assert!(
            s.record_cancel_request(id, "key-late")
                .await
                .unwrap()
                .is_none(),
            "a terminal job has nothing left to cancel"
        );
        let job = s.get(id).await.unwrap().unwrap();
        assert_eq!(job.cancelled_by.as_deref(), Some("key-op"));
    }

    /// A terminal verdict is never rewritten: the second writer reports
    /// `false` and the row keeps the first one's status, error and instant.
    #[sqlx::test]
    async fn finish_if_running_never_overwrites_a_terminal_row(pool: PgPool) {
        let s = store(&pool);
        let id = claim_running(&s).await;

        // The request has to be on the row before the verdict: the
        // migration refuses a `cancelled` status with no recorded asker.
        s.record_cancel_request(id, "key-op")
            .await
            .unwrap()
            .unwrap();
        assert!(
            s.finish_if_running(id, RepinJobStatus::Cancelled, Some("cancelled by key-op"))
                .await
                .unwrap()
        );
        let after_first = s.get(id).await.unwrap().unwrap();

        assert!(
            !s.finish_if_running(id, RepinJobStatus::Succeeded, None)
                .await
                .unwrap(),
            "the row is no longer running"
        );
        let after_second = s.get(id).await.unwrap().unwrap();
        assert_eq!(after_second.status, RepinJobStatus::Cancelled);
        assert_eq!(after_second.error.as_deref(), Some("cancelled by key-op"));
        assert_eq!(after_second.finished_at, after_first.finished_at);
    }

    /// The crash state (migration 0014): a job whose cancel was requested
    /// but never observed dies `running` and boot reconciliation fails it,
    /// request fields and all. The constraints must admit that row —
    /// `cancelled` is live-process-only, so recovery may not infer it from
    /// a populated `cancel_requested_at`.
    #[sqlx::test]
    async fn a_failed_job_may_carry_cancel_request_fields(pool: PgPool) {
        let s = store(&pool);
        let id = claim_running(&s).await;
        s.record_cancel_request(id, "key-op")
            .await
            .unwrap()
            .unwrap();

        let failed = s.reconcile_orphans(None).await.unwrap();
        assert_eq!(failed, 1);

        let job = s.get(id).await.unwrap().unwrap();
        assert_eq!(job.status, RepinJobStatus::Failed);
        assert_eq!(job.cancelled_by.as_deref(), Some("key-op"));
        assert!(job.cancel_requested_at.is_some());
    }

    /// The other direction of the same rule: `cancelled` without a recorded
    /// request is a status nothing asked for, and the database refuses it
    /// rather than storing a verdict with no author.
    #[sqlx::test]
    async fn cancelled_without_a_request_is_refused(pool: PgPool) {
        let s = store(&pool);
        let id = claim_running(&s).await;

        let err = sqlx::query("UPDATE repin_jobs SET status = 'cancelled' WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .expect_err("the CHECK refuses an unrequested cancellation");
        assert!(
            format!("{err}").contains("repin_jobs_cancelled_request_check"),
            "unexpected error: {err}"
        );

        // And the paired CHECK: neither column stands alone.
        let err = sqlx::query("UPDATE repin_jobs SET cancelled_by = 'key-op' WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .expect_err("an actor with no instant is half a fact");
        assert!(
            format!("{err}").contains("repin_jobs_cancel_request_check"),
            "unexpected error: {err}"
        );
    }
}
