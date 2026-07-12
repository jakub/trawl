// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres store tests for trawl-server's app-state stores (ADR-0004
//! slice 3).
//!
//! Ports every behaviour the retired trawl-auth sqlite unit tests pinned
//! (AC2), adds the pg-specific concurrency guarantees the sqlite stores got
//! for free from their process-wide mutex (AC3), and exercises the boot
//! path — advisory lock, migration, idempotency, failure modes (AC5).
//!
//! Plain `#[sqlx::test]` auto-applies `crates/trawl-server/migrations/` to
//! each per-test database; boot tests use `migrations = false` plus a
//! sibling database driven through `StorageState::connect` (the real path).

mod common;

use sqlx::PgPool;
use trawl_server::store::{
    HistoryStore, RunClaim, RunStatus, SavedQueryStore, ScheduleStore, StorageState, StoreError,
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
// history (AC2)
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
// saved queries (AC2)
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn saved_create_and_list(pool: PgPool) {
    let store = saved(&pool);
    let key_id = 1;

    let created = store
        .create(key_id, "nginx_errors", "service=nginx level=error")
        .await
        .unwrap();
    assert_eq!(created.name, "nginx_errors");
    assert_eq!(created.query, "service=nginx level=error");

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

    store.create(1, "my_query", "level=error").await.unwrap();

    let found = store.get_by_name(1, "my_query").await.unwrap().unwrap();
    assert_eq!(found.name, "my_query");
    assert_eq!(found.query, "level=error");

    // Other user can't see it.
    assert!(store.get_by_name(2, "my_query").await.unwrap().is_none());
}

/// The bulk-join list carries schedule + latest run + run count per item in
/// ONE statement (AC4's bounded-query-count contract).
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
        let rid = schedule_store
            .start_run(sched_a.id, a.id, "q1")
            .await
            .unwrap()
            .unwrap();
        assert!(
            schedule_store
                .finish_run(rid, RunStatus::Success, 100 + i, Some(5), None, None, None)
                .await
                .unwrap()
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
// schedules + runs (AC2)
// ---------------------------------------------------------------------------

async fn seed_saved(pool: &PgPool, key_id: i64, name: &str) -> i64 {
    saved(pool)
        .create(key_id, name, "level=error")
        .await
        .unwrap()
        .id
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
    // The store enforces the 60s minimum on both create and update paths,
    // not just in the handler's parse_interval.
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

/// Full cascade chain: deleting the saved query wipes its schedule AND its
/// runs, and the delete transaction reports the parquet paths to unlink.
#[sqlx::test]
async fn cascade_on_saved_query_delete(pool: PgPool) {
    let saved_store = saved(&pool);
    let store = schedules(&pool);
    let sq = saved_store.create(1, "test", "level=error").await.unwrap();
    let schedule = store.create_schedule(sq.id, 1, 300, None).await.unwrap();

    let run_id = store
        .start_run(schedule.id, sq.id, "level=error")
        .await
        .unwrap()
        .unwrap();
    assert!(
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
            .unwrap()
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

    let run_id = store
        .start_run(schedule.id, sq_id, "q")
        .await
        .unwrap()
        .unwrap();
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
// run lifecycle (AC2)
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn start_and_finish_run(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "test").await;
    let schedule = store.create_schedule(sq_id, 1, 300, None).await.unwrap();

    let run_id = store
        .start_run(schedule.id, sq_id, "level=error")
        .await
        .unwrap()
        .expect("should start run");

    let run = store.get_run(run_id, 1).await.unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Running);

    assert!(
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
            .unwrap()
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

    let first = store.start_run(schedule.id, sq_id, "q").await.unwrap();
    assert!(first.is_some());

    // Second start returns None (already running) via the named 23505.
    let second = store.start_run(schedule.id, sq_id, "q").await.unwrap();
    assert!(second.is_none());

    // After finishing, a new run can start.
    store
        .finish_run(
            first.unwrap(),
            RunStatus::Success,
            100,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    let third = store.start_run(schedule.id, sq_id, "q").await.unwrap();
    assert!(third.is_some());
}

#[sqlx::test]
async fn list_runs_paginated_and_isolated(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "test").await;
    let schedule = store.create_schedule(sq_id, 1, 300, None).await.unwrap();

    for i in 0usize..5 {
        let run_id = store
            .start_run(schedule.id, sq_id, "q")
            .await
            .unwrap()
            .unwrap();
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
    store.start_run(schedule.id, sq_id, "q").await.unwrap();

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

    let run_id = store
        .start_run(schedule.id, sq_id, "q1")
        .await
        .unwrap()
        .unwrap();
    store
        .finish_run(run_id, RunStatus::Success, 100, None, None, None, None)
        .await
        .unwrap();

    let run_id = store
        .start_run(schedule.id, sq_id, "q2")
        .await
        .unwrap()
        .unwrap();
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
        let rid = store
            .start_run(sched1.id, sq1, "q1")
            .await
            .unwrap()
            .unwrap();
        store
            .finish_run(rid, RunStatus::Success, 50, None, None, None, None)
            .await
            .unwrap();
    }
    for _ in 0..2 {
        let rid = store
            .start_run(sched2.id, sq2, "q2")
            .await
            .unwrap()
            .unwrap();
        store
            .finish_run(rid, RunStatus::Success, 80, None, None, None, None)
            .await
            .unwrap();
    }
    let rid = store
        .start_run(sched3.id, sq3, "q3")
        .await
        .unwrap()
        .unwrap();
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
        let rid = store
            .start_run(sched.id, sq_id, "q")
            .await
            .unwrap()
            .unwrap();
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
        let rid = store
            .start_run(sched.id, sq_id, "q")
            .await
            .unwrap()
            .unwrap();
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

    // Age cutoff 30 days AND keep at most 1 per schedule.
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
    let r1 = store
        .start_run(sched.id, sq_id, "q")
        .await
        .unwrap()
        .unwrap();
    store
        .finish_run(r1, RunStatus::Error, 10, None, Some("x"), None, None)
        .await
        .unwrap();
    let r2 = store
        .start_run(sched.id, sq_id, "q")
        .await
        .unwrap()
        .unwrap();
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
    let r3 = store
        .start_run(sched.id, sq_id, "q")
        .await
        .unwrap()
        .unwrap();
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
// concurrency (AC3) — the load-bearing new coverage: sqlite's atomicity was
// an accident of the process-wide mutex, pg must prove it.
// ---------------------------------------------------------------------------

/// Two connections racing `start_run` on one schedule: exactly one claims
/// (the partial unique index `report_runs_one_running` decides).
#[sqlx::test]
async fn concurrent_start_run_yields_exactly_one_claim(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "race").await;
    let sched = store.create_schedule(sq_id, 1, 300, None).await.unwrap();

    let s1 = store.clone();
    let s2 = store.clone();
    let (a, b) = tokio::join!(
        tokio::spawn(async move { s1.start_run(sched.id, sq_id, "q").await }),
        tokio::spawn(async move { s2.start_run(sched.id, sq_id, "q").await }),
    );
    let a = a.unwrap().unwrap();
    let b = b.unwrap().unwrap();

    assert!(
        a.is_some() ^ b.is_some(),
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
    let rid = store
        .start_run(sched.id, sq_id, "q")
        .await
        .unwrap()
        .unwrap();
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

    let rid = store
        .start_run(sched.id, sq.id, "q")
        .await
        .unwrap()
        .unwrap();

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
    assert!(
        !updated,
        "zero-row finish must be reported so the orphan file gets removed"
    );
}

/// `fail_run_if_running` is the ambiguous-commit recovery guard: it flips a
/// still-`running` row to `error`, but must NEVER clobber a run whose success
/// already committed. Regression for the finding where an unconditional retry
/// destroyed a committed result and orphaned its parquet file.
#[sqlx::test]
async fn fail_run_if_running_is_a_guarded_transition(pool: PgPool) {
    let saved_store = saved(&pool);
    let store = schedules(&pool);

    // A still-running run flips to error and the result path is cleared.
    let sq = saved_store.create(1, "running", "q").await.unwrap();
    let sched = store.create_schedule(sq.id, 1, 300, None).await.unwrap();
    let rid = store
        .start_run(sched.id, sq.id, "q")
        .await
        .unwrap()
        .unwrap();

    assert!(
        store
            .fail_run_if_running(rid, 5, "result persistence failed")
            .await
            .unwrap(),
        "a running row must be flipped to error"
    );
    let run = store.get_run(rid, 1).await.unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Error);
    assert_eq!(run.result_path, None);

    // A run whose success already committed must survive the guarded flip:
    // the ambiguous-commit case where the first finish_run's COMMIT landed.
    let sq2 = saved_store.create(1, "committed", "q").await.unwrap();
    let sched2 = store.create_schedule(sq2.id, 1, 300, None).await.unwrap();
    let rid2 = store
        .start_run(sched2.id, sq2.id, "q")
        .await
        .unwrap()
        .unwrap();
    assert!(
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
            .unwrap()
    );

    assert!(
        !store
            .fail_run_if_running(rid2, 5, "result persistence failed")
            .await
            .unwrap(),
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

/// Barrier-driven regression: a scheduler's `finish_run` landing its parquet
/// path while `SavedQueryStore::delete` is mid-flight must never orphan the
/// file. A blocker transaction pins the interleaving to the exact window the
/// finding describes — `finish_run` commits its path after `delete` collected
/// paths but before its cascade wipes the row. The fix locks the parent and
/// every run row first, so `delete` either collects the path or the run row
/// survives long enough for `finish_run` to report the orphan. Invariant:
/// `delete` returns the path iff `finish_run` succeeded — exactly one side owns
/// cleanup. The pre-fix code returns an empty set while `finish_run` reports
/// success, leaking the file, and trips this assertion.
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
    let rid = sched_store
        .start_run(sched.id, sq.id, "q")
        .await
        .unwrap()
        .unwrap();

    // Hold the parent row so `delete` stalls at the spot the race needs: the
    // fixed code blocks on its parent `FOR UPDATE`; the pre-fix code blocks on
    // its cascade `DELETE` — after it already read an empty path set.
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query_scalar::<_, i64>("SELECT id FROM saved_queries WHERE id = $1 FOR UPDATE")
        .bind(sq.id)
        .fetch_one(&mut *blocker)
        .await
        .unwrap();

    let delete_store = saved_store.clone();
    let sq_id = sq.id;
    let delete_task = tokio::spawn(async move { delete_store.delete(sq_id, 1).await });

    // Let `delete` reach its blocking point (so the pre-fix path read has
    // already run) before the scheduler commits its result path.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    let finished = sched_store
        .finish_run(rid, RunStatus::Success, 10, Some(1), None, None, Some(PATH))
        .await
        .unwrap();

    blocker.rollback().await.unwrap();
    let delete_paths = delete_task.await.unwrap().unwrap();

    assert!(
        finished,
        "finish_run committed its path before the cascade — it must report success"
    );
    assert_eq!(
        delete_paths.contains(&PATH.to_string()),
        finished,
        "exactly one side must own the parquet cleanup: delete returned {delete_paths:?}"
    );
    assert_eq!(sched_store.count_runs(sched.id).await.unwrap(), 0);
}

/// Same barrier-driven race for `ScheduleStore::delete_schedule`: the blocker
/// holds the schedule row, `finish_run` commits its path mid-delete, and the
/// fix guarantees `delete_schedule` collects it (rather than cascading it away
/// while `finish_run` believed it persisted).
#[sqlx::test]
async fn delete_schedule_racing_finish_run_never_orphans_path(pool: PgPool) {
    const PATH: &str = "scheduled/race/sched.parquet";
    let sched_store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "race").await;
    let sched = sched_store
        .create_schedule(sq_id, 1, 300, None)
        .await
        .unwrap();
    let rid = sched_store
        .start_run(sched.id, sq_id, "q")
        .await
        .unwrap()
        .unwrap();

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

    assert!(
        finished,
        "finish_run committed its path before the cascade — it must report success"
    );
    assert_eq!(
        delete_paths.contains(&PATH.to_string()),
        finished,
        "exactly one side must own the parquet cleanup: delete_schedule returned {delete_paths:?}"
    );
    assert_eq!(sched_store.count_runs(sched.id).await.unwrap(), 0);
}

// ---------------------------------------------------------------------------
// boot: advisory lock + migration (AC5)
// ---------------------------------------------------------------------------

/// Fresh empty database + `StorageState::connect` applies the schema via
/// the real boot path (advisory lock BEFORE migrate).
#[sqlx::test(migrations = false)]
async fn boot_migrates_fresh_database(pool: PgPool) {
    let url = common::create_app_database(&pool).await;
    let storage = StorageState::connect(&url).await.expect("first boot");

    // Schema is live: a store call succeeds.
    storage.saved.create(1, "boot", "q").await.unwrap();
    storage.ping().await.unwrap();
}

/// A second boot against an already-migrated database is a no-op (after the
/// first instance released its advisory lock).
#[sqlx::test(migrations = false)]
async fn boot_is_idempotent_after_shutdown(pool: PgPool) {
    let url = common::create_app_database(&pool).await;
    {
        let storage = StorageState::connect(&url).await.expect("first boot");
        storage.saved.create(1, "boot", "q").await.unwrap();
        drop(storage);
    }

    // The advisory lock releases when the first instance's session closes;
    // that is asynchronous, so retry briefly.
    let mut last_err = None;
    for _ in 0..50 {
        match StorageState::connect(&url).await {
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

/// A second LIVE instance on the same DSN fails startup on the advisory
/// lock with a descriptive error.
#[sqlx::test(migrations = false)]
async fn boot_second_live_instance_fails_on_advisory_lock(pool: PgPool) {
    let url = common::create_app_database(&pool).await;
    let _first = StorageState::connect(&url).await.expect("first boot");

    let err = StorageState::connect(&url)
        .await
        .expect_err("second live instance must fail");
    assert!(matches!(err, StoreError::LockHeld), "got: {err:?}");
    assert!(
        err.to_string().contains("advisory lock"),
        "error must name the advisory lock: {err}"
    );
}

/// An unreachable storage database is a descriptive startup failure.
#[sqlx::test(migrations = false)]
async fn boot_unreachable_database_fails_descriptively(pool: PgPool) {
    let _ = pool; // harness provides the env; the DSN below is dead on purpose
    let err = StorageState::connect("postgres://trawl:nope@127.0.0.1:1/trawl")
        .await
        .expect_err("dead DSN must fail");
    assert!(matches!(err, StoreError::Unavailable(_)), "got: {err:?}");
}

/// Losing the lock-holding session must be detected, and must free the lock
/// for a replacement — the split-brain guard from the [high] review finding.
///
/// Terminating every backend on the app database kills the dedicated
/// advisory-lock connection (postgres releases the session lock) while the
/// database survives. The original instance's guard must flip its lock-lost
/// signal (and report unhealthy on `/health`), and a *replacement* instance
/// must then be able to acquire the freed lock — proving the original can no
/// longer be trusted as sole writer.
#[sqlx::test(migrations = false)]
async fn lock_loss_is_detected_and_frees_the_lock_for_a_replacement(pool: PgPool) {
    use std::time::Duration;

    let url = common::create_app_database(&pool).await;
    let first = StorageState::connect(&url).await.expect("first boot");

    let mut lost = first.lock_lost();
    assert!(!*lost.borrow_and_update(), "lock is healthy at boot");
    first.ping_cached().await.expect("storage healthy at boot");

    // While the lock is held, a replacement cannot start.
    assert!(
        matches!(StorageState::connect(&url).await, Err(StoreError::LockHeld)),
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
        match StorageState::connect(&url).await {
            Ok(_replacement) => return,
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    panic!("replacement never acquired the freed lock: {last_err:?}");
}
