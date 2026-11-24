// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres store tests for trawl-server's app-state stores.
//!
//! The boot tests take no harness pool: they mint their own database and
//! drive it through `StorageState`, so `#[sqlx::test]` would only mint a
//! second database nobody opens and charge its 5-connection pool to the
//! admission budget (`common::BOOT_CONNECTION_CEILING`).

mod common;

use sqlx::{PgPool, Row};
use trawl_server::pool::RunLease;
use trawl_server::report_window::{ReportWindow, ScheduleWindow, WindowKind, truncate_to_micros};
use trawl_server::store::schedule::{DUE_FIRST_PAGE, DUE_RESUMED_PAGE};
use trawl_server::store::{
    CLAIM_WAIT, DUE_SCHEDULE_PAGE, DueClaim, DueClaimError, DueSchedule, FinishOutcome,
    FlipOutcome, HistoryStore, ManualRunClaim, RunClaim, RunStatus, SavedQueryStore, ScheduleStore,
    StorageState, StoreError, WindowWriteError,
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

    let page = store.get_user_history(key_id, 10, 0, None).await.unwrap();
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

    let page1 = store.get_user_history(key_id, 2, 0, None).await.unwrap();
    assert_eq!(page1.total, 5);
    assert_eq!(page1.entries.len(), 2);
    assert_eq!(page1.entries[0].query, "query 4");
    assert_eq!(page1.entries[1].query, "query 3");

    let page2 = store.get_user_history(key_id, 2, 2, None).await.unwrap();
    assert_eq!(page2.entries[0].query, "query 2");
    assert_eq!(page2.entries[1].query, "query 1");

    let page3 = store.get_user_history(key_id, 2, 4, None).await.unwrap();
    assert_eq!(page3.entries.len(), 1);
    assert_eq!(page3.entries[0].query, "query 0");
}

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
    let page1 = store.get_user_history(1, 2, 0, None).await.unwrap();
    let page2 = store.get_user_history(1, 2, 2, None).await.unwrap();
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

    let user1 = store.get_user_history(1, 10, 0, None).await.unwrap();
    assert_eq!(user1.total, 2);
    assert_eq!(user1.entries[0].query, "user 1 query 2");

    let user2 = store.get_user_history(2, 10, 0, None).await.unwrap();
    assert_eq!(user2.total, 1);
    assert_eq!(user2.entries[0].query, "user 2 query");
}

#[sqlx::test]
async fn history_empty(pool: PgPool) {
    let store = history(&pool);
    let page = store.get_user_history(999, 10, 0, None).await.unwrap();
    assert_eq!(page.total, 0);
    assert_eq!(page.entries.len(), 0);
}

#[sqlx::test]
async fn history_filter_counts_and_orders_across_all_key_history(pool: PgPool) {
    let store = history(&pool);
    let mut matching_ids = Vec::new();
    for index in 0..120 {
        let query = if index % 3 == 0 {
            format!("unrelated query {index}")
        } else {
            format!("address 198.51.100.7 query {index}")
        };
        // Every timestamp ties; interleaved foreign ids must not affect pages.
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO query_history (key_id, query, executed_at, duration_ms, row_count, status)
             VALUES (1, $1, '2026-07-01T00:00:00Z', 1, 1, 'success') RETURNING id",
        )
        .bind(&query)
        .fetch_one(&pool)
        .await
        .unwrap();
        if index % 3 != 0 {
            matching_ids.push(id);
        }
        sqlx::query(
            "INSERT INTO query_history (key_id, query, executed_at, duration_ms, row_count, status)
             VALUES (2, $1, '2026-07-01T00:00:00Z', 1, 1, 'success')",
        )
        .bind(&query)
        .execute(&pool)
        .await
        .unwrap();
    }
    matching_ids.reverse();
    let mut observed_ids = Vec::new();
    for (offset, expected_len) in [(0, 50), (50, 30), (100, 0)] {
        let page = store
            .get_user_history(1, 50, offset, Some("198.51.100.7"))
            .await
            .unwrap();
        assert_eq!(page.total, 80);
        assert_eq!(page.entries.len(), expected_len);
        assert!(page.entries.iter().all(|entry| entry.key_id == 1));
        observed_ids.extend(page.entries.iter().map(|entry| entry.id));
    }
    assert_eq!(observed_ids, matching_ids);
    let unfiltered = store.get_user_history(1, 50, 0, None).await.unwrap();
    assert_eq!(unfiltered.total, 120);
    assert_eq!(
        store.get_user_history(1, 50, 0, Some("")).await.unwrap(),
        unfiltered
    );
    assert!(
        !unfiltered
            .entries
            .iter()
            .any(|entry| entry.id == *matching_ids.last().unwrap())
    );
    let absent = store
        .get_user_history(1, 50, 0, Some("no match anywhere"))
        .await
        .unwrap();
    assert_eq!(absent.total, 0);
    assert!(absent.entries.is_empty());
}

#[sqlx::test]
async fn history_filter_preserves_literal_punctuation_and_spaces(pool: PgPool) {
    let store = history(&pool);
    let query = "IP 192.0.2.1 two  spaces %_\\.*[x]$ + apostrophe' END";
    let id = store
        .record_query(1, query, 1, 1, RunStatus::Success)
        .await
        .unwrap();
    store
        .record_query(1, "ordinary other query", 1, 1, RunStatus::Success)
        .await
        .unwrap();
    store
        .record_query(2, query, 1, 1, RunStatus::Success)
        .await
        .unwrap();
    for needle in [
        "ip 192.0.2.1",
        "two  spaces",
        "%_",
        "\\",
        ".*[x]$",
        "+",
        "apostrophe'",
        " END",
    ] {
        let page = store
            .get_user_history(1, 50, 0, Some(needle))
            .await
            .unwrap();
        assert_eq!(page.total, 1, "literal needle {needle:?}");
        assert_eq!(page.entries[0].id, id);
    }
    for needle in [
        "two spaces",
        "END ",
        "' OR 1=1 --",
        "%ordinary%",
        "192.0.2._",
    ] {
        assert_eq!(
            store
                .get_user_history(1, 50, 0, Some(needle))
                .await
                .unwrap()
                .total,
            0,
            "literal needle {needle:?}"
        );
    }
}

#[sqlx::test]
async fn history_filter_non_ascii_follows_reported_postgres_locale(pool: PgPool) {
    let store = history(&pool);
    let locale: (String, String, String, Option<String>) = sqlx::query_as(
        "SELECT datcollate, datctype, datlocprovider::text, datlocale
                       FROM pg_database WHERE datname = current_database()",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    eprintln!(
        "History lowercase test: datcollate={}, datctype={}, provider={}, locale={:?}",
        locale.0, locale.1, locale.2, locale.3
    );
    let queries = [
        "ÄPFEL",
        "ÉCOLE",
        "E\u{301}COLE",
        "東京",
        "İSTANBUL",
        "Straße",
    ];
    let mut lowered = Vec::new();
    for query in queries {
        let id = store
            .record_query(1, query, 1, 1, RunStatus::Success)
            .await
            .unwrap();
        let text: String = sqlx::query_scalar("SELECT lower($1::text)")
            .bind(query)
            .fetch_one(&pool)
            .await
            .unwrap();
        lowered.push((id, text));
    }
    for needle in [
        "äpfel",
        "école",
        "e\u{301}cole",
        "東京",
        "istanbul",
        "STRASSE",
    ] {
        // Ask PostgreSQL only for lowercase conversion. Rust's literal byte
        // substring comparison then supplies the independent expected ids.
        let lower_needle: String = sqlx::query_scalar("SELECT lower($1::text)")
            .bind(needle)
            .fetch_one(&pool)
            .await
            .unwrap();
        let expected: Vec<_> = lowered
            .iter()
            .rev()
            .filter(|(_, text)| text.contains(&lower_needle))
            .map(|(id, _)| *id)
            .collect();
        let page = store
            .get_user_history(1, 50, 0, Some(needle))
            .await
            .unwrap();
        assert_eq!(
            page.total,
            expected.len(),
            "locale {locale:?}, needle {needle:?}"
        );
        assert_eq!(
            page.entries
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            expected
        );
    }
}

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
        .update_checked(created.id, 1, "updated query", None)
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
        .update_checked(created.id, 1, "query", Some("new-name"))
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
    let result = store.update_checked(other.id, 1, "q", Some("taken")).await;
    assert!(matches!(
        result,
        Err(WindowWriteError::Store(StoreError::DuplicateName { .. }))
    ));
}

#[sqlx::test]
async fn saved_update_nonexistent_returns_error(pool: PgPool) {
    let store = saved(&pool);
    let result = store.update_checked(999, 1, "query", None).await;
    assert!(matches!(
        result,
        Err(WindowWriteError::Store(StoreError::NotFound { .. }))
    ));
}

#[sqlx::test]
async fn saved_update_other_users_query_returns_error(pool: PgPool) {
    let store = saved(&pool);
    let created = store.create(1, "test", "query").await.unwrap();
    let result = store.update_checked(created.id, 2, "updated", None).await;
    assert!(matches!(
        result,
        Err(WindowWriteError::Store(StoreError::NotFound { .. }))
    ));
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
    for bad in [
        "",
        "   ",
        "a\nb",
        "\tname",
        "name\0",
        "name\u{7f}",
        "\u{200b}",
        "a\u{202e}b",
        "👩\u{200d}💻",
    ] {
        assert!(
            matches!(
                store.create(1, bad, "q").await,
                Err(StoreError::InvalidName { .. })
            ),
            "{bad:?} must be rejected"
        );
    }
    for good in [
        "daily_ip_rollup",
        "hourly-error-count",
        "Auth2",
        "a",
        "has spaces",
        "foo/bar",
        "雪 \"報告\" \\ folder",
    ] {
        store.create(1, good, "q").await.unwrap();
    }
    let created = store
        .create(1, "  A  readable / name  ", "q")
        .await
        .unwrap();
    assert_eq!(created.name, "A  readable / name");
    assert!(matches!(
        store.create(1, "A  readable / name", "q").await,
        Err(StoreError::DuplicateName { .. })
    ));
    store.create(2, "A  readable / name", "q").await.unwrap();
    store.create(1, "a  readable / name", "q").await.unwrap();
    let renamed = store
        .update_checked(created.id, 1, "q", Some("  Renamed / 雪  "))
        .await
        .unwrap();
    assert_eq!(renamed.id, created.id);
    assert_eq!(renamed.name, "Renamed / 雪");
    assert!(
        store
            .get_by_name(1, "A  readable / name")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .get_by_name(1, "Renamed / 雪")
            .await
            .unwrap()
            .unwrap()
            .id,
        created.id
    );
    assert!(
        store
            .update_checked(created.id, 1, "q", Some("a\nb"))
            .await
            .is_err()
    );
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

    assert!(store.get_by_name(2, "my_query").await.unwrap().is_none());
}

#[sqlx::test]
async fn saved_list_with_details_bulk_join(pool: PgPool) {
    let saved_store = saved(&pool);
    let schedule_store = schedules(&pool);

    let a = saved_store.create(1, "alpha", "q1").await.unwrap();
    let b = saved_store.create(1, "beta", "q2").await.unwrap();
    saved_store.create(1, "gamma", "q3").await.unwrap();

    let sched_a = schedule_store
        .create_schedule(a.id, 1, 300, Some(10), None, 0, chrono::Utc::now())
        .await
        .unwrap();
    schedule_store
        .create_schedule(b.id, 1, 600, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();

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
    assert_eq!(alpha.schedule.runs_claimed, 2);
    let latest = alpha.latest_run.as_ref().expect("latest run present");
    assert_eq!(latest.duration_ms, Some(101), "latest run is the newest");

    let beta = details[1].schedule.as_ref().expect("beta has schedule");
    assert_eq!(beta.schedule.runs_claimed, 0);
    assert!(beta.latest_run.is_none());

    assert!(details[2].schedule.is_none(), "gamma has no schedule");
}

async fn seed_saved(pool: &PgPool, key_id: i64, name: &str) -> i64 {
    saved(pool)
        .create(key_id, name, "_severity=error")
        .await
        .unwrap()
        .id
}

/// Claim a `running` report run and return its id, asserting the claim
/// started. It spends one run of the schedule's budget, like every claim.
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
async fn claim_run_locks_saved_query_before_schedule(pool: PgPool) {
    let store = schedules(&pool);
    let saved_id = seed_saved(&pool, 1, "claim-lock-order").await;
    let schedule = store
        .create_schedule(saved_id, 1, 300, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();
    let mut editor = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM saved_queries WHERE id = $1 FOR UPDATE")
        .bind(saved_id)
        .execute(&mut *editor)
        .await
        .unwrap();

    let claim =
        tokio::spawn(async move { store.claim_run(schedule.id, saved_id, "q", None).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity
                 WHERE datname = current_database() AND wait_event_type = 'Lock'
                   AND pid <> pg_backend_pid())",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if waiting {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("claim must reach the held saved-query lock");

    let schedule_lock = sqlx::query("SELECT id FROM schedules WHERE id = $1 FOR UPDATE NOWAIT")
        .bind(schedule.id)
        .execute(&mut *editor)
        .await;
    editor.rollback().await.unwrap();
    let claimed = claim.await.unwrap().unwrap();
    assert!(
        schedule_lock.is_ok(),
        "claim took schedule before saved query: {schedule_lock:?}"
    );
    assert!(matches!(claimed, RunClaim::Started(_)));
}

#[sqlx::test]
async fn schedule_create_and_get(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "test-query").await;

    let schedule = store
        .create_schedule(sq_id, 1, 300, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();
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

    store
        .create_schedule(sq_id, 1, 300, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();
    let result = store
        .create_schedule(sq_id, 1, 600, None, None, 0, chrono::Utc::now())
        .await;
    assert!(matches!(result, Err(StoreError::ScheduleExists { .. })));
}

#[sqlx::test]
async fn schedule_for_missing_saved_query_is_not_found(pool: PgPool) {
    let store = schedules(&pool);
    let result = store
        .create_schedule(12345, 1, 300, None, None, 0, chrono::Utc::now())
        .await;
    assert!(matches!(result, Err(StoreError::NotFound { .. })));
}

#[sqlx::test]
async fn schedule_rejects_sub_minute_interval(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "test").await;

    assert!(matches!(
        store
            .create_schedule(sq_id, 1, 30, None, None, 0, chrono::Utc::now())
            .await,
        Err(StoreError::IntervalTooShort { secs: 30 })
    ));

    let created = store
        .create_schedule(sq_id, 1, 300, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();
    assert!(matches!(
        store
            .update_schedule(created.id, 1, 45, None, true, None, 0, chrono::Utc::now())
            .await,
        Err(StoreError::IntervalTooShort { secs: 45 })
    ));
}

#[sqlx::test]
async fn schedule_update(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "test").await;

    let created = store
        .create_schedule(sq_id, 1, 300, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();
    let updated = store
        .update_schedule(
            created.id,
            1,
            600,
            Some(10),
            false,
            None,
            0,
            chrono::Utc::now(),
        )
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
    let created = store
        .create_schedule(sq_id, 1, 300, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();
    let result = store
        .update_schedule(created.id, 2, 600, None, true, None, 0, chrono::Utc::now())
        .await;
    assert!(matches!(result, Err(StoreError::NotFound { .. })));
}

fn violation(e: sqlx::Error) -> (Option<String>, Option<String>) {
    match e {
        sqlx::Error::Database(db) => (
            db.code().map(|c| c.to_string()),
            db.constraint().map(str::to_owned),
        ),
        other => panic!("expected a database error, got {other:?}"),
    }
}

async fn raw_cursors(
    pool: &PgPool,
    id: i64,
) -> (
    Option<String>,
    Option<i64>,
    i64,
    Option<chrono::DateTime<chrono::Utc>>,
    chrono::DateTime<chrono::Utc>,
) {
    sqlx::query_as(
        "SELECT window_kind, window_secs, lag_secs, covered_through, next_fire_at
         FROM schedules WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[sqlx::test]
async fn schedule_window_round_trips(pool: PgPool) {
    let store = schedules(&pool);
    let now = truncate_to_micros(chrono::Utc::now());

    for (name, window, lag) in [
        ("legacy", None, 0),
        ("tiled", Some(ScheduleWindow::SinceLast), 300),
        ("trailing", Some(ScheduleWindow::Fixed { secs: 7200 }), 0),
    ] {
        let sq_id = seed_saved(&pool, 1, name).await;
        let created = store
            .create_schedule(sq_id, 1, 300, None, window, lag, now)
            .await
            .unwrap();
        assert_eq!(created.window, window, "{name}: created window");
        assert_eq!(created.lag_secs, lag, "{name}: created lag");
        assert_eq!(
            created.covered_through,
            match window {
                // A since_last schedule owes coverage from its first
                // window's start, and says so before it has ever run.
                Some(ScheduleWindow::SinceLast) => Some(now - chrono::Duration::seconds(600)),
                _ => None,
            },
            "{name}: the created watermark"
        );

        let fetched = store
            .get_schedule_for_saved_query(sq_id, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched.window, window, "{name}: refetched window");
        assert_eq!(fetched.lag_secs, lag, "{name}: refetched lag");

        let (kind, secs, ..) = raw_cursors(&pool, created.id).await;
        assert_eq!(
            kind.as_deref(),
            window.map(|w| w.kind().as_str()),
            "{name}: persisted kind"
        );
        assert_eq!(
            secs,
            window
                .and_then(ScheduleWindow::secs)
                .map(i64::try_from)
                .transpose()
                .unwrap()
        );
    }
}

#[sqlx::test]
async fn schedule_create_anchors_next_fire_at_to_caller_instant(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "anchored").await;
    let now = truncate_to_micros(chrono::Utc::now()) - chrono::Duration::hours(3);

    let created = store
        .create_schedule(sq_id, 1, 300, None, Some(ScheduleWindow::SinceLast), 0, now)
        .await
        .unwrap();
    assert_eq!(created.next_fire_at, now);

    let (.., next_fire_at) = raw_cursors(&pool, created.id).await;
    assert_eq!(next_fire_at, now);
}

#[sqlx::test]
async fn schedule_update_reanchors_only_on_cadence_change(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "reanchor").await;
    let t0 = truncate_to_micros(chrono::Utc::now());
    let at = |mins: i64| t0 + chrono::Duration::minutes(mins);

    let created = store
        .create_schedule(sq_id, 1, 300, None, None, 0, t0)
        .await
        .unwrap();
    assert_eq!(created.next_fire_at, t0);

    let same = store
        .update_schedule(created.id, 1, 300, Some(5), false, None, 0, at(1))
        .await
        .unwrap();
    assert_eq!(same.next_fire_at, t0, "max_runs/enabled must not re-anchor");

    let interval = store
        .update_schedule(created.id, 1, 600, Some(5), false, None, 0, at(2))
        .await
        .unwrap();
    assert_eq!(interval.next_fire_at, at(2), "interval change re-anchors");

    let windowed = store
        .update_schedule(
            created.id,
            1,
            600,
            Some(5),
            false,
            Some(ScheduleWindow::SinceLast),
            0,
            at(3),
        )
        .await
        .unwrap();
    assert_eq!(windowed.next_fire_at, at(3), "window change re-anchors");

    let lagged = store
        .update_schedule(
            created.id,
            1,
            600,
            Some(5),
            false,
            Some(ScheduleWindow::SinceLast),
            120,
            at(4),
        )
        .await
        .unwrap();
    assert_eq!(lagged.next_fire_at, at(3), "lag change must not re-anchor");
    assert_eq!(lagged.lag_secs, 120);
}

#[sqlx::test]
async fn schedule_update_preserves_covered_through(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "watermark").await;
    let now = truncate_to_micros(chrono::Utc::now());
    let created = store
        .create_schedule(sq_id, 1, 300, None, Some(ScheduleWindow::SinceLast), 0, now)
        .await
        .unwrap();

    let mark = now - chrono::Duration::hours(1);
    sqlx::query("UPDATE schedules SET covered_through = $1 WHERE id = $2")
        .bind(mark)
        .bind(created.id)
        .execute(&pool)
        .await
        .unwrap();

    for (interval, window, enabled) in [
        (300, Some(ScheduleWindow::SinceLast), true),
        (900, Some(ScheduleWindow::Fixed { secs: 900 }), false),
        (900, None, true),
    ] {
        let updated = store
            .update_schedule(created.id, 1, interval, None, enabled, window, 0, now)
            .await
            .unwrap();
        assert_eq!(
            updated.covered_through,
            Some(mark),
            "edit ({interval}, {window:?}) must leave the watermark alone"
        );
    }
}

#[sqlx::test]
async fn since_last_watermark_is_seeded_at_the_origin_of_owed_coverage(pool: PgPool) {
    let store = schedules(&pool);
    let now = truncate_to_micros(chrono::Utc::now());
    let back = |secs: i64| Some(now - chrono::Duration::seconds(secs));

    for (name, window, lag, expected) in [
        ("tiled", Some(ScheduleWindow::SinceLast), 0, back(3600)),
        ("lagged", Some(ScheduleWindow::SinceLast), 300, back(3900)),
        (
            "trailing",
            Some(ScheduleWindow::Fixed { secs: 7200 }),
            0,
            None,
        ),
        ("query", None, 0, None),
    ] {
        let sq_id = seed_saved(&pool, 1, name).await;
        let created = store
            .create_schedule(sq_id, 1, 3600, None, window, lag, now)
            .await
            .unwrap();
        assert_eq!(created.covered_through, expected, "{name}: created origin");
    }

    let sq_id = seed_saved(&pool, 1, "retyped").await;
    let sched = store
        .create_schedule(sq_id, 1, 3600, None, None, 0, now)
        .await
        .unwrap();
    assert_eq!(sched.covered_through, None);

    let retyped_at = now + chrono::Duration::hours(2);
    let retyped = store
        .update_schedule(
            sched.id,
            1,
            3600,
            None,
            true,
            Some(ScheduleWindow::SinceLast),
            0,
            retyped_at,
        )
        .await
        .unwrap();
    assert_eq!(retyped.next_fire_at, retyped_at);
    assert_eq!(
        retyped.covered_through,
        Some(retyped_at - chrono::Duration::hours(1)),
        "retyping into since_last seeds the new origin"
    );

    let advanced = retyped_at + chrono::Duration::hours(3);
    sqlx::query("UPDATE schedules SET covered_through = $1 WHERE id = $2")
        .bind(advanced)
        .bind(sched.id)
        .execute(&pool)
        .await
        .unwrap();
    let re_edited = store
        .update_schedule(
            sched.id,
            1,
            900,
            None,
            true,
            Some(ScheduleWindow::SinceLast),
            0,
            retyped_at + chrono::Duration::hours(4),
        )
        .await
        .unwrap();
    assert_eq!(
        re_edited.covered_through,
        Some(advanced),
        "coverage a run earned survives every later edit"
    );

    // An edit that does NOT re-anchor seeds nothing. Nothing this code writes
    // leaves a since_last schedule watermark-less, so the row is forced there.
    sqlx::query("UPDATE schedules SET covered_through = NULL WHERE id = $1")
        .bind(sched.id)
        .execute(&pool)
        .await
        .unwrap();
    let untouched = store
        .update_schedule(
            sched.id,
            1,
            900,
            Some(7),
            false,
            Some(ScheduleWindow::SinceLast),
            0,
            retyped_at + chrono::Duration::hours(5),
        )
        .await
        .unwrap();
    assert_eq!(
        untouched.covered_through, None,
        "max_runs and enabled are not a new origin"
    );
}

#[sqlx::test]
async fn schedule_lag_without_window_is_refused(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "lagless").await;
    let now = chrono::Utc::now();

    assert!(matches!(
        store
            .create_schedule(sq_id, 1, 300, None, None, 60, now)
            .await,
        Err(StoreError::LagWithoutWindow { lag_secs: 60 })
    ));

    let created = store
        .create_schedule(
            sq_id,
            1,
            300,
            None,
            Some(ScheduleWindow::SinceLast),
            60,
            now,
        )
        .await
        .unwrap();
    assert!(matches!(
        store
            .update_schedule(created.id, 1, 300, None, true, None, 60, now)
            .await,
        Err(StoreError::LagWithoutWindow { lag_secs: 60 })
    ));
    let unchanged = store
        .get_schedule_for_saved_query(sq_id, 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.window, Some(ScheduleWindow::SinceLast));
    assert_eq!(unchanged.lag_secs, 60);
}

#[sqlx::test]
async fn claimed_run_round_trips_its_window(pool: PgPool) {
    let store = schedules(&pool);
    let now = truncate_to_micros(chrono::Utc::now());

    let windowed_sq = seed_saved(&pool, 1, "windowed").await;
    let sched = store
        .create_schedule(
            windowed_sq,
            1,
            300,
            None,
            Some(ScheduleWindow::SinceLast),
            0,
            now,
        )
        .await
        .unwrap();
    let window = ReportWindow {
        start: now - chrono::Duration::hours(2),
        end: now,
        truncated: true,
        kind: WindowKind::SinceLast,
    };
    let rid = match store
        .claim_run(sched.id, windowed_sq, "q", Some(&window))
        .await
        .unwrap()
    {
        RunClaim::Started(id) => id,
        other => panic!("expected a started run, got {other:?}"),
    };
    let run = store.get_run(rid, 1).await.unwrap().unwrap();
    assert_eq!(run.window_start, Some(window.start));
    assert_eq!(run.window_end, Some(window.end));
    assert_eq!(run.window_truncated, Some(true));
    assert_eq!(run.window_kind, Some(WindowKind::SinceLast));

    let legacy_sq = seed_saved(&pool, 1, "legacy-run").await;
    let legacy_sched = store
        .create_schedule(legacy_sq, 1, 300, None, None, 0, now)
        .await
        .unwrap();
    let legacy_rid = seed_run(&store, legacy_sched.id, legacy_sq, "q").await;
    let legacy = store.get_run(legacy_rid, 1).await.unwrap().unwrap();
    assert_eq!(legacy.window_start, None);
    assert_eq!(legacy.window_end, None);
    assert_eq!(
        legacy.window_truncated, None,
        "a run with no window claims nothing, not completeness"
    );
    assert_eq!(legacy.window_kind, None);
}

#[sqlx::test]
async fn since_last_watermark_advances_only_on_success(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "tiling").await;
    let now = truncate_to_micros(chrono::Utc::now());
    // Anchored well before the windows below, so every one of them ends
    // after the seeded origin and the only thing under test is which
    // completion moves the watermark.
    let created_at = now - chrono::Duration::hours(10);
    let sched = store
        .create_schedule(
            sq_id,
            1,
            300,
            None,
            Some(ScheduleWindow::SinceLast),
            0,
            created_at,
        )
        .await
        .unwrap();
    let seed = created_at - chrono::Duration::seconds(300);

    let covered = async || {
        store
            .get_schedule_for_saved_query(sq_id, 1)
            .await
            .unwrap()
            .unwrap()
            .covered_through
    };
    let window = |from_hours: i64, to_hours: i64| ReportWindow {
        start: now - chrono::Duration::hours(from_hours),
        end: now - chrono::Duration::hours(to_hours),
        truncated: false,
        kind: WindowKind::SinceLast,
    };
    let run = async |w: &ReportWindow| match store
        .claim_run(sched.id, sq_id, "q", Some(w))
        .await
        .unwrap()
    {
        RunClaim::Started(id) => id,
        other => panic!("expected a started run, got {other:?}"),
    };

    let failed = run(&window(4, 3)).await;
    store
        .finish_run(failed, RunStatus::Error, 5, None, Some("boom"), None, None)
        .await
        .unwrap();
    assert_eq!(
        covered().await,
        Some(seed),
        "a failed run leaves the seeded origin standing"
    );

    let timed_out = run(&window(4, 3)).await;
    store
        .finish_run(timed_out, RunStatus::Timeout, 5, None, None, None, None)
        .await
        .unwrap();
    assert_eq!(
        covered().await,
        Some(seed),
        "a timed-out run leaves the seeded origin standing"
    );

    let ok = window(4, 3);
    let ok_id = run(&ok).await;
    store
        .finish_run(ok_id, RunStatus::Success, 5, Some(3), None, None, None)
        .await
        .unwrap();
    assert_eq!(covered().await, Some(ok.end));

    let later = window(3, 2);
    let later_id = run(&later).await;
    store
        .finish_run(later_id, RunStatus::Success, 5, Some(1), None, None, None)
        .await
        .unwrap();
    assert_eq!(covered().await, Some(later.end));

    let stale = window(9, 8);
    let stale_id = run(&stale).await;
    store
        .finish_run(stale_id, RunStatus::Success, 5, Some(1), None, None, None)
        .await
        .unwrap();
    assert_eq!(
        covered().await,
        Some(later.end),
        "an older window must not rewind coverage"
    );
}

#[sqlx::test]
async fn finish_and_window_mode_edit_are_order_independent(pool: PgPool) {
    let store = schedules(&pool);
    let now = truncate_to_micros(chrono::Utc::now());
    let bounds = |kind: WindowKind| ReportWindow {
        start: now - chrono::Duration::hours(1),
        end: now,
        truncated: false,
        kind,
    };

    let covered_after = async |name: &str,
                               claimed: ScheduleWindow,
                               edited_to: ScheduleWindow,
                               finish_first: bool| {
        let sq_id = seed_saved(&pool, 1, name).await;
        let sched = store
            .create_schedule(sq_id, 1, 300, None, Some(claimed), 0, now)
            .await
            .unwrap();
        let window = bounds(claimed.kind());
        let rid = match store
            .claim_run(sched.id, sq_id, "q", Some(&window))
            .await
            .unwrap()
        {
            RunClaim::Started(id) => id,
            other => panic!("expected a started run, got {other:?}"),
        };

        let finish = async || {
            store
                .finish_run(rid, RunStatus::Success, 5, Some(1), None, None, None)
                .await
                .unwrap();
        };
        let edit = async || {
            store
                .update_schedule(sched.id, 1, 300, None, true, Some(edited_to), 0, now)
                .await
                .unwrap();
        };
        if finish_first {
            finish().await;
            edit().await;
        } else {
            edit().await;
            finish().await;
        }

        store
            .get_schedule_for_saved_query(sq_id, 1)
            .await
            .unwrap()
            .unwrap()
            .covered_through
    };

    let since_last = ScheduleWindow::SinceLast;
    let fixed = ScheduleWindow::Fixed { secs: 3600 };
    let finish_then_edit = covered_after("tiled-a", since_last, fixed, true).await;
    let edit_then_finish = covered_after("tiled-b", since_last, fixed, false).await;
    assert_eq!(
        finish_then_edit, edit_then_finish,
        "a since_last run's coverage must not depend on commit order"
    );
    assert_eq!(finish_then_edit, Some(now), "and it must be the run's end");

    let finish_then_edit = covered_after("trailing-a", fixed, since_last, true).await;
    let edit_then_finish = covered_after("trailing-b", fixed, since_last, false).await;
    let seed = now - chrono::Duration::seconds(300);
    assert_eq!(
        (finish_then_edit, edit_then_finish),
        (Some(seed), Some(seed)),
        "a fixed run never moves a watermark, whichever commit lands first"
    );
    assert_ne!(
        finish_then_edit,
        Some(now),
        "and the fixed run's own end is not what landed there"
    );
}

#[sqlx::test]
async fn fixed_and_legacy_schedules_keep_no_watermark(pool: PgPool) {
    let store = schedules(&pool);
    let now = truncate_to_micros(chrono::Utc::now());

    for (name, window) in [
        ("trailing", Some(ScheduleWindow::Fixed { secs: 3600 })),
        ("legacy", None),
    ] {
        let sq_id = seed_saved(&pool, 1, name).await;
        let sched = store
            .create_schedule(sq_id, 1, 300, None, window, 0, now)
            .await
            .unwrap();
        let covered = window.map(|w| ReportWindow {
            start: now - chrono::Duration::hours(1),
            end: now,
            truncated: false,
            kind: w.kind(),
        });
        let rid = match store
            .claim_run(sched.id, sq_id, "q", covered.as_ref())
            .await
            .unwrap()
        {
            RunClaim::Started(id) => id,
            other => panic!("expected a started run, got {other:?}"),
        };
        store
            .finish_run(rid, RunStatus::Success, 5, Some(1), None, None, None)
            .await
            .unwrap();

        let after = store
            .get_schedule_for_saved_query(sq_id, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after.covered_through, None,
            "{name}: only a since_last run keeps a watermark"
        );
        let run = store.get_run(rid, 1).await.unwrap().unwrap();
        assert_eq!(
            run.window_end,
            covered.map(|w| w.end),
            "{name}: the run still records what it covered"
        );
        assert_eq!(run.window_kind, window.map(ScheduleWindow::kind));
    }
}

/// How many intervals of catch-up these tests allow. Large enough that
/// nothing here is truncated by accident.
const CATCHUP: u32 = 24;

async fn reread(store: &ScheduleStore, sq_id: i64) -> trawl_server::store::Schedule {
    store
        .get_schedule_for_saved_query(sq_id, 1)
        .await
        .unwrap()
        .unwrap()
}

#[sqlx::test]
async fn claim_due_run_is_not_due_before_the_cursor(pool: PgPool) {
    let store = schedules(&pool);
    let now = truncate_to_micros(chrono::Utc::now());
    let sq_id = seed_saved(&pool, 1, "early").await;
    let sched = store
        .create_schedule(sq_id, 1, 3600, None, None, 0, now)
        .await
        .unwrap();

    let a_second_early = now - chrono::Duration::seconds(1);
    assert_eq!(
        store
            .claim_due_run(sched.id, a_second_early, CATCHUP, &RunLease::unattached())
            .await
            .unwrap(),
        DueClaim::NotDue
    );
    assert_eq!(store.count_runs(sched.id).await.unwrap(), 0);
    assert_eq!(
        reread(&store, sq_id).await.next_fire_at,
        now,
        "a not-due tick writes nothing at all"
    );

    assert!(matches!(
        store
            .claim_due_run(sched.id, now, CATCHUP, &RunLease::unattached())
            .await
            .unwrap(),
        DueClaim::Started(_)
    ));
}

#[sqlx::test]
async fn claim_due_run_advances_over_ground_the_watermark_covers(pool: PgPool) {
    let store = schedules(&pool);
    let now = truncate_to_micros(chrono::Utc::now());
    let sq_id = seed_saved(&pool, 1, "covered").await;
    let sched = store
        .create_schedule(
            sq_id,
            1,
            3600,
            None,
            Some(ScheduleWindow::SinceLast),
            0,
            now,
        )
        .await
        .unwrap();

    // Only an edit can produce this shape, so plant it the way an edit would.
    sqlx::query("UPDATE schedules SET covered_through = $1 WHERE id = $2")
        .bind(now + chrono::Duration::hours(10))
        .bind(sched.id)
        .execute(&pool)
        .await
        .unwrap();

    assert_eq!(
        store
            .claim_due_run(sched.id, now, CATCHUP, &RunLease::unattached())
            .await
            .unwrap(),
        DueClaim::Advanced
    );
    assert_eq!(store.count_runs(sched.id).await.unwrap(), 0);
    assert_eq!(
        reread(&store, sq_id).await.next_fire_at,
        now + chrono::Duration::hours(1),
        "the cursor moves one interval so the tick stops re-asking"
    );
    assert_eq!(
        runs_claimed(&pool, sched.id).await,
        0,
        "moving the cursor claims no run, so it spends none"
    );
}

#[sqlx::test]
async fn claim_due_run_leaves_the_cursor_while_a_run_is_in_flight(pool: PgPool) {
    let store = schedules(&pool);
    let now = truncate_to_micros(chrono::Utc::now());
    let sq_id = seed_saved(&pool, 1, "inflight").await;
    let sched = store
        .create_schedule(
            sq_id,
            1,
            3600,
            None,
            Some(ScheduleWindow::SinceLast),
            0,
            now,
        )
        .await
        .unwrap();

    assert!(matches!(
        store
            .claim_due_run(sched.id, now, CATCHUP, &RunLease::unattached())
            .await
            .unwrap(),
        DueClaim::Started(_)
    ));
    let after_first = reread(&store, sq_id).await.next_fire_at;
    assert_eq!(after_first, now + chrono::Duration::hours(1));

    assert_eq!(
        store
            .claim_due_run(sched.id, after_first, CATCHUP, &RunLease::unattached())
            .await
            .unwrap(),
        DueClaim::AlreadyRunning
    );
    assert_eq!(store.count_runs(sched.id).await.unwrap(), 1);
    assert_eq!(
        reread(&store, sq_id).await.next_fire_at,
        after_first,
        "a refused boundary must stay claimable"
    );
}

#[sqlx::test]
async fn claim_due_run_leaves_the_cursor_at_the_max_runs_cap(pool: PgPool) {
    let store = schedules(&pool);
    let now = truncate_to_micros(chrono::Utc::now());
    let sq_id = seed_saved(&pool, 1, "capped").await;
    let sched = store
        .create_schedule(sq_id, 1, 3600, Some(1), None, 0, now)
        .await
        .unwrap();

    let DueClaim::Started(first) = store
        .claim_due_run(sched.id, now, CATCHUP, &RunLease::unattached())
        .await
        .unwrap()
    else {
        panic!("the first run is under the cap");
    };
    store
        .finish_run(
            first.run_id,
            RunStatus::Success,
            5,
            Some(0),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    let after_first = reread(&store, sq_id).await.next_fire_at;

    assert_eq!(
        store
            .claim_due_run(sched.id, after_first, CATCHUP, &RunLease::unattached())
            .await
            .unwrap(),
        DueClaim::MaxRunsReached
    );
    assert_eq!(store.count_runs(sched.id).await.unwrap(), 1);
    assert_eq!(reread(&store, sq_id).await.next_fire_at, after_first);
}

#[sqlx::test]
async fn claim_due_run_is_not_due_when_disabled_or_deleted(pool: PgPool) {
    let store = schedules(&pool);
    let now = truncate_to_micros(chrono::Utc::now());
    let sq_id = seed_saved(&pool, 1, "paused").await;
    let sched = store
        .create_schedule(sq_id, 1, 3600, None, None, 0, now)
        .await
        .unwrap();
    store
        .update_schedule(sched.id, 1, 3600, None, false, None, 0, now)
        .await
        .unwrap();

    let later = now + chrono::Duration::hours(5);
    assert_eq!(
        store
            .claim_due_run(sched.id, later, CATCHUP, &RunLease::unattached())
            .await
            .unwrap(),
        DueClaim::NotDue,
        "a disabled schedule owes nothing"
    );
    assert_eq!(
        reread(&store, sq_id).await.next_fire_at,
        now,
        "and its cursor is not moved while it is off"
    );

    store.delete_schedule(sq_id, 1).await.unwrap();
    assert_eq!(
        store
            .claim_due_run(sched.id, later, CATCHUP, &RunLease::unattached())
            .await
            .unwrap(),
        DueClaim::NotDue,
        "a deleted schedule is not an error, just nothing to do"
    );
}

#[sqlx::test]
async fn claim_due_run_stores_the_resolved_query_it_returns(pool: PgPool) {
    let store = schedules(&pool);
    let now = truncate_to_micros(chrono::Utc::now());
    let sq_id = saved(&pool)
        .create(1, "resolved", "service=fx | table _time")
        .await
        .unwrap()
        .id;
    let sched = store
        .create_schedule(
            sq_id,
            1,
            3600,
            None,
            Some(ScheduleWindow::SinceLast),
            0,
            now,
        )
        .await
        .unwrap();

    let DueClaim::Started(claimed) = store
        .claim_due_run(sched.id, now, CATCHUP, &RunLease::unattached())
        .await
        .unwrap()
    else {
        panic!("the schedule is due at its own creation instant");
    };

    let window = claimed.window.expect("a windowed schedule claims a window");
    assert_eq!(window.kind, WindowKind::SinceLast);
    assert_eq!(window.end, now, "no lag, so the window ends at the fire");
    assert_eq!(
        window.start,
        now - chrono::Duration::hours(1),
        "the first since_last run covers one interval (ruling 14)"
    );
    assert!(!window.truncated);
    assert!(
        claimed.resolved_query.ends_with("service=fx | table _time"),
        "the saved text survives verbatim: {}",
        claimed.resolved_query
    );
    assert!(claimed.resolved_query.starts_with("earliest="));
    assert_eq!(claimed.query_name, "resolved");
    assert_eq!(claimed.saved_query_id, sq_id);

    let run = store.get_run(claimed.run_id, 1).await.unwrap().unwrap();
    assert_eq!(
        run.query, claimed.resolved_query,
        "the stored text and the executed text are one string"
    );
    assert_eq!(
        (run.window_start, run.window_end),
        (Some(window.start), Some(window.end))
    );
    assert_eq!(run.window_truncated, Some(false));
    assert_eq!(run.window_kind, Some(WindowKind::SinceLast));
    assert_eq!(
        reread(&store, sq_id).await.next_fire_at,
        now + chrono::Duration::hours(1)
    );
}

#[sqlx::test]
async fn claim_due_run_refuses_a_query_that_owns_its_own_window(pool: PgPool) {
    let store = schedules(&pool);
    let now = truncate_to_micros(chrono::Utc::now());
    let sq_id = saved(&pool)
        .create(1, "conflicted", "service=fx | table _time")
        .await
        .unwrap()
        .id;
    let sched = store
        .create_schedule(
            sq_id,
            1,
            3600,
            None,
            Some(ScheduleWindow::SinceLast),
            0,
            now,
        )
        .await
        .unwrap();
    // Write the forbidden pair the only way that is still possible: past
    // the write-time gate, straight at the column.
    sqlx::query("UPDATE saved_queries SET query = $1 WHERE id = $2")
        .bind("service=fx last=1h | table _time")
        .bind(sq_id)
        .execute(&pool)
        .await
        .unwrap();

    let err = store
        .claim_due_run(sched.id, now, CATCHUP, &RunLease::unattached())
        .await
        .expect_err("a windowed schedule cannot execute a query that owns its own window");
    assert!(
        matches!(err, DueClaimError::Policy(_)),
        "expected the window-policy refusal, got {err:?}"
    );
    assert_eq!(err.class(), "window_policy");
    assert!(
        err.to_string().contains("last="),
        "the message names the clause it found: {err}"
    );

    assert_eq!(store.count_runs(sched.id).await.unwrap(), 0);
    assert_eq!(
        reread(&store, sq_id).await.next_fire_at,
        now,
        "a refused claim leaves the cursor, so the state is loud every poll"
    );
}

async fn runs_claimed(pool: &PgPool, schedule_id: i64) -> i64 {
    sqlx::query_scalar("SELECT runs_claimed FROM schedules WHERE id = $1")
        .bind(schedule_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[derive(Debug, Clone, Copy)]
enum Claimant {
    /// The scheduler tick, [`ScheduleStore::claim_due_run`].
    Due,
    /// An operator, [`ScheduleStore::claim_manual_run`].
    Manual,
    /// The test seeding door, [`ScheduleStore::claim_run`].
    Seed,
}

const CLAIMANTS: [Claimant; 3] = [Claimant::Due, Claimant::Manual, Claimant::Seed];

#[derive(Debug, PartialEq, Eq)]
enum Claimed {
    Started(i64),
    AlreadyRunning,
    MaxRunsReached,
    Other(String),
}

async fn claim_via(
    store: &ScheduleStore,
    claimant: Claimant,
    sq_id: i64,
) -> Result<Claimed, String> {
    let schedule = reread(store, sq_id).await;
    match claimant {
        Claimant::Due => match store
            .claim_due_run(
                schedule.id,
                schedule.next_fire_at,
                CATCHUP,
                &RunLease::unattached(),
            )
            .await
        {
            Ok(DueClaim::Started(claimed)) => Ok(Claimed::Started(claimed.run_id)),
            Ok(DueClaim::AlreadyRunning) => Ok(Claimed::AlreadyRunning),
            Ok(DueClaim::MaxRunsReached) => Ok(Claimed::MaxRunsReached),
            Ok(other) => Ok(Claimed::Other(format!("{other:?}"))),
            Err(e) => Err(format!("{e:?}")),
        },
        Claimant::Manual => match store
            .claim_manual_run(sq_id, 1, CATCHUP, &RunLease::unattached())
            .await
        {
            Ok(ManualRunClaim::Started(claimed)) => Ok(Claimed::Started(claimed.run_id)),
            Ok(ManualRunClaim::AlreadyRunning) => Ok(Claimed::AlreadyRunning),
            Ok(ManualRunClaim::MaxRunsReached) => Ok(Claimed::MaxRunsReached),
            Ok(other) => Ok(Claimed::Other(format!("{other:?}"))),
            Err(e) => Err(format!("{e:?}")),
        },
        Claimant::Seed => match store
            .claim_run(schedule.id, sq_id, "_severity=error", None)
            .await
        {
            Ok(RunClaim::Started(run_id)) => Ok(Claimed::Started(run_id)),
            Ok(RunClaim::AlreadyRunning) => Ok(Claimed::AlreadyRunning),
            Ok(RunClaim::MaxRunsReached) => Ok(Claimed::MaxRunsReached),
            Err(e) => Err(format!("{e:?}")),
        },
    }
}

async fn start_via(store: &ScheduleStore, claimant: Claimant, sq_id: i64) -> i64 {
    match claim_via(store, claimant, sq_id).await {
        Ok(Claimed::Started(run_id)) => run_id,
        other => panic!("{claimant:?} must start a run, got {other:?}"),
    }
}

async fn finish(store: &ScheduleStore, run_id: i64, status: RunStatus) {
    let message = (status != RunStatus::Success).then_some("planted failure");
    let outcome = store
        .finish_run(run_id, status, 5, Some(0), message, None, None)
        .await
        .unwrap();
    assert_eq!(outcome, FinishOutcome::Persisted);
}

#[sqlx::test]
async fn run_budget_is_spent_by_every_claimant(pool: PgPool) {
    let store = schedules(&pool);
    let now = truncate_to_micros(chrono::Utc::now());
    let sq_id = seed_saved(&pool, 1, "spent by all").await;
    let sched = store
        .create_schedule(sq_id, 1, 3600, Some(3), None, 0, now)
        .await
        .unwrap();

    for (spent, claimant) in (1..).zip(CLAIMANTS) {
        let run_id = start_via(&store, claimant, sq_id).await;
        assert_eq!(
            runs_claimed(&pool, sched.id).await,
            spent,
            "{claimant:?} spends exactly one run"
        );
        finish(&store, run_id, RunStatus::Success).await;
    }

    for claimant in CLAIMANTS {
        assert_eq!(
            claim_via(&store, claimant, sq_id).await,
            Ok(Claimed::MaxRunsReached),
            "{claimant:?} refuses a spent budget"
        );
    }
    assert_eq!(
        runs_claimed(&pool, sched.id).await,
        3,
        "a refusal spends nothing"
    );
    assert_eq!(store.count_runs(sched.id).await.unwrap(), 3);
    assert_eq!(
        reread(&store, sq_id).await.updated_at,
        sched.updated_at,
        "spending the budget is not an edit"
    );
}

#[sqlx::test]
async fn run_budget_reads_the_counter_not_the_rows(pool: PgPool) {
    let store = schedules(&pool);
    let now = truncate_to_micros(chrono::Utc::now());

    let spent_sq = seed_saved(&pool, 1, "spent, nothing kept").await;
    let spent = store
        .create_schedule(spent_sq, 1, 3600, Some(2), None, 0, now)
        .await
        .unwrap();
    sqlx::query("UPDATE schedules SET runs_claimed = 2 WHERE id = $1")
        .bind(spent.id)
        .execute(&pool)
        .await
        .unwrap();
    for claimant in CLAIMANTS {
        assert_eq!(
            claim_via(&store, claimant, spent_sq).await,
            Ok(Claimed::MaxRunsReached),
            "{claimant:?} refuses with no runs to count"
        );
    }
    assert_eq!(store.count_runs(spent.id).await.unwrap(), 0);
    assert_eq!(runs_claimed(&pool, spent.id).await, 2);
    assert_eq!(
        reread(&store, spent_sq).await.next_fire_at,
        now,
        "the refused boundary stays claimable"
    );

    for claimant in CLAIMANTS {
        let sq_id = seed_saved(&pool, 1, &format!("one left for {claimant:?}")).await;
        let sched = store
            .create_schedule(sq_id, 1, 3600, None, None, 0, now)
            .await
            .unwrap();
        for _ in 0..3 {
            let run_id = seed_run(&store, sched.id, sq_id, "q").await;
            finish(&store, run_id, RunStatus::Success).await;
        }
        store
            .update_schedule(sched.id, 1, 3600, Some(2), true, None, 0, now)
            .await
            .unwrap();
        sqlx::query("UPDATE schedules SET runs_claimed = 1 WHERE id = $1")
            .bind(sched.id)
            .execute(&pool)
            .await
            .unwrap();

        let run_id = start_via(&store, claimant, sq_id).await;
        assert_eq!(runs_claimed(&pool, sched.id).await, 2, "{claimant:?}");
        finish(&store, run_id, RunStatus::Success).await;
        for refused in CLAIMANTS {
            assert_eq!(
                claim_via(&store, refused, sq_id).await,
                Ok(Claimed::MaxRunsReached),
                "{refused:?} refuses once {claimant:?} spent the last run"
            );
        }
        assert_eq!(store.count_runs(sched.id).await.unwrap(), 4);
        assert_eq!(runs_claimed(&pool, sched.id).await, 2);
    }
}

#[sqlx::test]
async fn refused_claims_spend_no_budget(pool: PgPool) {
    let store = schedules(&pool);
    let now = truncate_to_micros(chrono::Utc::now());

    // A run in flight. A manual claim asks before it spends; the other two
    // learn it from the insert, after the spend.
    let busy_sq = seed_saved(&pool, 1, "busy").await;
    let busy = store
        .create_schedule(busy_sq, 1, 3600, Some(5), None, 0, now)
        .await
        .unwrap();
    seed_run(&store, busy.id, busy_sq, "q").await;
    for claimant in CLAIMANTS {
        assert_eq!(
            claim_via(&store, claimant, busy_sq).await,
            Ok(Claimed::AlreadyRunning),
            "{claimant:?}"
        );
    }
    assert_eq!(runs_claimed(&pool, busy.id).await, 1);
    assert_eq!(store.count_runs(busy.id).await.unwrap(), 1);

    let covered_sq = seed_saved(&pool, 1, "covered").await;
    let covered = store
        .create_schedule(
            covered_sq,
            1,
            3600,
            Some(5),
            Some(ScheduleWindow::SinceLast),
            0,
            now,
        )
        .await
        .unwrap();
    sqlx::query("UPDATE schedules SET covered_through = $1 WHERE id = $2")
        .bind(now + chrono::Duration::hours(10))
        .bind(covered.id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        matches!(
            store
                .claim_manual_run(covered_sq, 1, CATCHUP, &RunLease::unattached())
                .await
                .unwrap(),
            ManualRunClaim::EmptyWindow { .. }
        ),
        "the watermark covers the manual window"
    );
    assert_eq!(runs_claimed(&pool, covered.id).await, 0);
    assert_eq!(store.count_runs(covered.id).await.unwrap(), 0);

    let conflicted_sq = saved(&pool)
        .create(1, "conflicted", "service=fx | table _time")
        .await
        .unwrap()
        .id;
    let conflicted = store
        .create_schedule(
            conflicted_sq,
            1,
            3600,
            Some(5),
            Some(ScheduleWindow::SinceLast),
            0,
            now,
        )
        .await
        .unwrap();
    sqlx::query("UPDATE saved_queries SET query = $1 WHERE id = $2")
        .bind("service=fx last=1h | table _time")
        .bind(conflicted_sq)
        .execute(&pool)
        .await
        .unwrap();
    for claimant in [Claimant::Due, Claimant::Manual] {
        let error = claim_via(&store, claimant, conflicted_sq)
            .await
            .expect_err("the window cannot be resolved");
        assert!(error.starts_with("Policy("), "{claimant:?}: {error}");
    }
    assert_eq!(runs_claimed(&pool, conflicted.id).await, 0);
    assert_eq!(store.count_runs(conflicted.id).await.unwrap(), 0);
    assert_eq!(reread(&store, conflicted_sq).await.next_fire_at, now);

    let failing_sq = seed_saved(&pool, 1, "failing insert").await;
    let failing = store
        .create_schedule(failing_sq, 1, 3600, Some(5), None, 0, now)
        .await
        .unwrap();
    sqlx::raw_sql(
        "CREATE FUNCTION refuse_report_run() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'injected report run failure'; END $$;
         CREATE TRIGGER refuse_report_run BEFORE INSERT ON report_runs
         FOR EACH ROW EXECUTE FUNCTION refuse_report_run();",
    )
    .execute(&pool)
    .await
    .unwrap();
    for claimant in CLAIMANTS {
        let error = claim_via(&store, claimant, failing_sq)
            .await
            .expect_err("the insert fails");
        assert!(
            error.contains("injected report run failure"),
            "{claimant:?}: {error}"
        );
    }
    assert_eq!(runs_claimed(&pool, failing.id).await, 0);
    assert_eq!(store.count_runs(failing.id).await.unwrap(), 0);
    assert_eq!(reread(&store, failing_sq).await.next_fire_at, now);

    sqlx::raw_sql("DROP TRIGGER refuse_report_run ON report_runs")
        .execute(&pool)
        .await
        .unwrap();
    start_via(&store, Claimant::Due, failing_sq).await;
    assert_eq!(runs_claimed(&pool, failing.id).await, 1);
}

#[sqlx::test]
async fn failed_runs_keep_their_claim(pool: PgPool) {
    let store = schedules(&pool);
    let now = truncate_to_micros(chrono::Utc::now());
    let sq_id = seed_saved(&pool, 1, "failing runs").await;
    let sched = store
        .create_schedule(sq_id, 1, 3600, Some(3), None, 0, now)
        .await
        .unwrap();

    let errored = start_via(&store, Claimant::Due, sq_id).await;
    finish(&store, errored, RunStatus::Error).await;
    let timed_out = start_via(&store, Claimant::Manual, sq_id).await;
    finish(&store, timed_out, RunStatus::Timeout).await;
    let interrupted = start_via(&store, Claimant::Seed, sq_id).await;
    assert_eq!(store.cleanup_stale_runs().await.unwrap(), 1);

    let interrupted = store.get_run(interrupted, 1).await.unwrap().unwrap();
    assert_eq!(interrupted.status, RunStatus::Error);
    assert_eq!(
        interrupted.error_message.as_deref(),
        Some("interrupted by server restart")
    );
    for (run_id, status) in [(errored, RunStatus::Error), (timed_out, RunStatus::Timeout)] {
        assert_eq!(
            store.get_run(run_id, 1).await.unwrap().unwrap().status,
            status
        );
    }

    assert_eq!(runs_claimed(&pool, sched.id).await, 3);
    for claimant in CLAIMANTS {
        assert_eq!(
            claim_via(&store, claimant, sq_id).await,
            Ok(Claimed::MaxRunsReached),
            "{claimant:?}: failed runs were still claims"
        );
    }
    assert_eq!(runs_claimed(&pool, sched.id).await, 3);
}

async fn put_schedule(
    store: &ScheduleStore,
    sq_id: i64,
    interval_secs: u64,
    max_runs: Option<u64>,
    now: chrono::DateTime<chrono::Utc>,
) -> trawl_server::store::Schedule {
    store
        .set_schedule_checked(sq_id, 1, interval_secs, max_runs, true, None, 0, now)
        .await
        .unwrap()
}

#[sqlx::test]
async fn run_budget_survives_edits_and_resets_on_recreate(pool: PgPool) {
    let store = schedules(&pool);
    let now = truncate_to_micros(chrono::Utc::now());
    let sq_id = seed_saved(&pool, 1, "edited").await;
    let sched = put_schedule(&store, sq_id, 3600, Some(2), now).await;
    for claimant in [Claimant::Due, Claimant::Seed] {
        let run_id = start_via(&store, claimant, sq_id).await;
        finish(&store, run_id, RunStatus::Success).await;
    }
    assert_eq!(runs_claimed(&pool, sched.id).await, 2);

    let edited = put_schedule(&store, sq_id, 7200, Some(2), now).await;
    assert_eq!(runs_claimed(&pool, sched.id).await, 2, "an edit keeps it");
    assert_eq!(
        (edited.runs_claimed, edited.exhausted()),
        (2, true),
        "the edit's answer reads the kept budget"
    );
    for claimant in CLAIMANTS {
        assert_eq!(
            claim_via(&store, claimant, sq_id).await,
            Ok(Claimed::MaxRunsReached),
            "{claimant:?}: an edit is not a new budget"
        );
    }

    let raised = put_schedule(&store, sq_id, 7200, Some(3), now).await;
    assert!(!raised.exhausted());
    let running = start_via(&store, Claimant::Manual, sq_id).await;
    assert_eq!(runs_claimed(&pool, sched.id).await, 3);

    let lowered = put_schedule(&store, sq_id, 7200, Some(1), now).await;
    assert_eq!(runs_claimed(&pool, sched.id).await, 3);
    assert_eq!(
        (lowered.runs_claimed, lowered.max_runs, lowered.exhausted()),
        (3, Some(1), true),
        "spent past a lowered cap is still spent"
    );
    for claimant in [Claimant::Due, Claimant::Seed] {
        assert_eq!(
            claim_via(&store, claimant, sq_id).await,
            Ok(Claimed::MaxRunsReached),
            "{claimant:?}"
        );
    }
    assert_eq!(
        claim_via(&store, Claimant::Manual, sq_id).await,
        Ok(Claimed::AlreadyRunning),
        "a run in flight still outranks the cap"
    );
    finish(&store, running, RunStatus::Success).await;
    assert_eq!(
        store.get_run(running, 1).await.unwrap().unwrap().status,
        RunStatus::Success
    );
    for claimant in CLAIMANTS {
        assert_eq!(
            claim_via(&store, claimant, sq_id).await,
            Ok(Claimed::MaxRunsReached),
            "{claimant:?}"
        );
    }
    assert_eq!(runs_claimed(&pool, sched.id).await, 3);

    store.delete_schedule(sq_id, 1).await.unwrap();
    let recreated = put_schedule(&store, sq_id, 7200, Some(1), now).await;
    assert_ne!(recreated.id, sched.id);
    assert_eq!(runs_claimed(&pool, recreated.id).await, 0);
    assert_eq!((recreated.runs_claimed, recreated.exhausted()), (0, false));
    start_via(&store, Claimant::Seed, sq_id).await;
    assert_eq!(runs_claimed(&pool, recreated.id).await, 1);
    assert!(
        reread(&store, sq_id).await.exhausted(),
        "a read decodes the spend"
    );
}

/// A CHECK accepts NULL as readily as TRUE, so a shape constraint written
/// as an OR of arms has a hole wherever an arm can evaluate to NULL.
#[sqlx::test]
async fn window_shape_rejects_null_kind_with_span(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "two-valued").await;
    let sched = store
        .create_schedule(sq_id, 1, 300, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();

    let expected = (
        Some("23514".to_owned()),
        Some("schedules_window_shape".to_owned()),
    );

    let err =
        sqlx::query("UPDATE schedules SET window_kind = NULL, window_secs = 60 WHERE id = $1")
            .bind(sched.id)
            .execute(&pool)
            .await
            .unwrap_err();
    assert_eq!(violation(err), expected, "a span with no mode");

    let err =
        sqlx::query("UPDATE schedules SET window_kind = 'fixed', window_secs = NULL WHERE id = $1")
            .bind(sched.id)
            .execute(&pool)
            .await
            .unwrap_err();
    assert_eq!(violation(err), expected, "fixed with no span");

    let err = sqlx::query(
        "INSERT INTO report_runs
             (schedule_id, saved_query_id, query, status, started_at,
              window_start, window_end, window_truncated)
         VALUES ($1, $2, 'q', 'error', now(), now() - INTERVAL '1 hour', now(), FALSE)",
    )
    .bind(sched.id)
    .bind(sq_id)
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(
        violation(err),
        (
            Some("23514".to_owned()),
            Some("report_runs_window_shape".to_owned())
        ),
        "a run window with no mode"
    );
}

#[sqlx::test]
async fn window_check_constraints_reject_partial_shapes(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "checks").await;
    let now = chrono::Utc::now();
    let sched = store
        .create_schedule(sq_id, 1, 300, None, None, 0, now)
        .await
        .unwrap();

    let err = sqlx::query(
        "INSERT INTO report_runs
             (schedule_id, saved_query_id, query, status, started_at, window_start, window_kind)
         VALUES ($1, $2, 'q', 'error', now(), now(), 'since_last')",
    )
    .bind(sched.id)
    .bind(sq_id)
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(
        violation(err),
        (
            Some("23514".to_owned()),
            Some("report_runs_window_shape".to_owned())
        )
    );

    let err = sqlx::query(
        "INSERT INTO report_runs
             (schedule_id, saved_query_id, query, status, started_at,
              window_start, window_end, window_truncated, window_kind)
         VALUES ($1, $2, 'q', 'error', now(), now(), now() - INTERVAL '1 hour', FALSE, 'since_last')",
    )
    .bind(sched.id)
    .bind(sq_id)
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(
        violation(err),
        (
            Some("23514".to_owned()),
            Some("report_runs_window_shape".to_owned())
        )
    );

    let err = sqlx::query("UPDATE schedules SET window_kind = 'fixed' WHERE id = $1")
        .bind(sched.id)
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(
        violation(err),
        (
            Some("23514".to_owned()),
            Some("schedules_window_shape".to_owned())
        )
    );

    let err =
        sqlx::query("UPDATE schedules SET window_kind = 'fixed', window_secs = 30 WHERE id = $1")
            .bind(sched.id)
            .execute(&pool)
            .await
            .unwrap_err();
    assert_eq!(
        violation(err),
        (
            Some("23514".to_owned()),
            Some("schedules_window_shape".to_owned())
        )
    );
}

#[sqlx::test]
async fn schedule_delete(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "test").await;
    store
        .create_schedule(sq_id, 1, 300, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();

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
    store
        .create_schedule(sq_id, 1, 300, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();
    let result = store.delete_schedule(sq_id, 2).await;
    assert!(matches!(result, Err(StoreError::NotFound { .. })));
}

#[sqlx::test]
async fn schedule_user_isolation(pool: PgPool) {
    let store = schedules(&pool);
    let sq1 = seed_saved(&pool, 1, "user1-query").await;
    let sq2 = seed_saved(&pool, 2, "user2-query").await;

    store
        .create_schedule(sq1, 1, 300, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();
    store
        .create_schedule(sq2, 2, 600, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();

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

#[sqlx::test]
async fn cascade_on_saved_query_delete(pool: PgPool) {
    let saved_store = saved(&pool);
    let store = schedules(&pool);
    let sq = saved_store
        .create(1, "test", "_severity=error")
        .await
        .unwrap();
    let schedule = store
        .create_schedule(sq.id, 1, 300, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();

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
    let schedule = store
        .create_schedule(sq_id, 1, 300, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();

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

#[sqlx::test]
async fn start_and_finish_run(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "test").await;
    let schedule = store
        .create_schedule(sq_id, 1, 300, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();

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
    let schedule = store
        .create_schedule(sq_id, 1, 300, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();

    let first = store
        .claim_run(schedule.id, sq_id, "q", None)
        .await
        .unwrap();
    let RunClaim::Started(first_id) = first else {
        panic!("first claim must start a run, got {first:?}");
    };

    let second = store
        .claim_run(schedule.id, sq_id, "q", None)
        .await
        .unwrap();
    assert!(matches!(second, RunClaim::AlreadyRunning), "got {second:?}");

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
    let schedule = store
        .create_schedule(sq_id, 1, 300, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();

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

    assert!(store.list_runs(sq_id, 2, 100, 0).await.unwrap().is_empty());
    assert_eq!(store.count_runs_for_saved_query(sq_id, 1).await.unwrap(), 5);
    assert_eq!(store.count_runs_for_saved_query(sq_id, 2).await.unwrap(), 0);
}

#[sqlx::test]
async fn cleanup_stale_runs_marks_error(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "test").await;
    let schedule = store
        .create_schedule(sq_id, 1, 300, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();

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
    let schedule = store
        .create_schedule(sq_id, 1, 300, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();

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

async fn set_runs_claimed(pool: &PgPool, id: i64, runs: i64) {
    sqlx::query("UPDATE schedules SET runs_claimed = $2 WHERE id = $1")
        .bind(id)
        .bind(runs)
        .execute(pool)
        .await
        .unwrap();
}

async fn list_all_due(
    store: &ScheduleStore,
    now: chrono::DateTime<chrono::Utc>,
) -> (Vec<(chrono::DateTime<chrono::Utc>, i64, i64)>, Vec<usize>) {
    let mut listed = Vec::new();
    let mut sizes = Vec::new();
    let mut after = None;
    loop {
        let rows = store
            .list_due_schedule_page(now, after, 10 * DUE_SCHEDULE_PAGE)
            .await
            .unwrap();
        sizes.push(rows.len());
        let Some(last) = rows.last() else { break };
        after = Some((last.next_fire_at, last.id));
        for row in rows {
            let DueSchedule {
                id,
                key_id,
                next_fire_at,
            } = row;
            listed.push((next_fire_at, id, key_id));
        }
    }
    (listed, sizes)
}

#[sqlx::test]
async fn due_schedule_pages_order_and_resume(pool: PgPool) {
    let store = schedules(&pool);
    let now = truncate_to_micros(chrono::Utc::now());

    let mut by_index = Vec::new();
    let mut spent = Vec::new();
    for i in 0..142_i64 {
        let fire = if i == 141 {
            now
        } else {
            now - chrono::Duration::minutes((i * 29) % 37 + 1)
        };
        let key_id = 1 + i % 3;
        let cap = (i % 3 == 0).then_some(5);
        let sq = seed_saved(&pool, key_id, &format!("due-{i}")).await;
        let schedule = store
            .create_schedule(sq, key_id, 3600, cap, None, 0, fire)
            .await
            .unwrap();
        if cap.is_some() {
            // 0 to 4 of 5 spent: room left.
            set_runs_claimed(&pool, schedule.id, i % 5).await;
        }
        by_index.push((fire, schedule.id, key_id));
        if i % 5 == 2 {
            let (cap, runs) = match (i / 5) % 3 {
                0 => (3, 3),
                1 => (2, 5),
                _ => (0, 0),
            };
            let sq = seed_saved(&pool, key_id, &format!("spent-{i}")).await;
            let schedule = store
                .create_schedule(sq, key_id, 3600, Some(cap), None, 0, fire)
                .await
                .unwrap();
            set_runs_claimed(&pool, schedule.id, runs).await;
            spent.push(((fire, schedule.id, key_id), cap, runs));
        }
    }
    let off = seed_saved(&pool, 1, "due-but-disabled").await;
    let off = store
        .create_schedule(
            off,
            1,
            3600,
            None,
            None,
            0,
            now - chrono::Duration::hours(1),
        )
        .await
        .unwrap();
    store
        .update_schedule(off.id, 1, 3600, None, false, None, 0, now)
        .await
        .unwrap();
    let future = seed_saved(&pool, 1, "not-yet-due").await;
    store
        .create_schedule(
            future,
            1,
            3600,
            None,
            None,
            0,
            now + chrono::Duration::microseconds(1),
        )
        .await
        .unwrap();

    let mut expected = by_index.clone();
    expected.sort_unstable();
    let page = DUE_SCHEDULE_PAGE;
    assert_eq!(
        (expected[page - 1].0, expected[2 * page - 1].0),
        (expected[page].0, expected[2 * page].0),
        "the fixture ties a cursor across each page boundary"
    );
    assert!(
        spent
            .iter()
            .any(|((fire, ..), ..)| [expected[page].0, expected[2 * page].0].contains(fire)),
        "the fixture ties a spent schedule on a page boundary's cursor"
    );
    for runs in [3, 5, 0] {
        assert!(
            spent.iter().any(|(_, _, r)| *r == runs),
            "the fixture has every kind of spent schedule"
        );
    }
    assert!(
        !expected.is_sorted_by_key(|(_, id, _)| *id),
        "the fixture's cursors are out of id order"
    );

    let (listed, sizes) = list_all_due(&store, now).await;
    assert_eq!(sizes, [page, page, 142 - 2 * page, 0], "page sizes");
    assert_eq!(listed, expected);

    let (first, second) = (expected[0], expected[1]);
    assert_eq!(
        store
            .list_due_schedule_page(now, Some((first.0, first.1)), 1)
            .await
            .unwrap(),
        [DueSchedule {
            id: second.1,
            key_id: second.2,
            next_fire_at: second.0,
        }],
        "a smaller limit is a smaller page, resumed after the key"
    );
    // Spent schedules are still enabled: the budget leaves the listing only.
    assert_eq!(
        store.count_enabled_schedules().await.unwrap(),
        u64::try_from(143 + spent.len()).unwrap()
    );

    let find = |runs: i64| *spent.iter().find(|(_, _, r)| *r == runs).unwrap();
    let (at_cap, ..) = find(3);
    let (past_cap, ..) = find(5);
    let (zero, ..) = find(0);
    for (row, cap) in [(at_cap, Some(4)), (past_cap, None), (zero, Some(1))] {
        store
            .update_schedule(row.1, row.2, 3600, cap, true, None, 0, now)
            .await
            .unwrap();
        expected.push(row);
    }
    // by_index[3] is capped at 5 with 3 spent; by_index[1] is unlimited.
    let (lowered, toggled) = (by_index[3], by_index[1]);
    store
        .update_schedule(lowered.1, lowered.2, 3600, Some(3), true, None, 0, now)
        .await
        .unwrap();
    store
        .update_schedule(toggled.1, toggled.2, 3600, None, false, None, 0, now)
        .await
        .unwrap();
    expected.retain(|row| ![lowered, toggled].contains(row));
    expected.sort_unstable();
    assert_eq!(
        list_all_due(&store, now).await.0,
        expected,
        "raised, removed and lowered caps and a disable, at unmoved cursors"
    );
    store
        .update_schedule(toggled.1, toggled.2, 3600, None, true, None, 0, now)
        .await
        .unwrap();
    expected.push(toggled);
    expected.sort_unstable();
    assert_eq!(
        list_all_due(&store, now).await.0,
        expected,
        "re-enabled at its own cursor"
    );

    let sq = seed_saved(&pool, 1, "cap-edit-race").await;
    let raced = store
        .create_schedule(
            sq,
            1,
            3600,
            Some(5),
            Some(ScheduleWindow::SinceLast),
            0,
            now - chrono::Duration::minutes(1),
        )
        .await
        .unwrap();
    set_runs_claimed(&pool, raced.id, 4).await;
    let before = raw_cursors(&pool, raced.id).await;
    assert!(
        list_all_due(&store, now)
            .await
            .0
            .contains(&(raced.next_fire_at, raced.id, 1)),
        "one run left: listed"
    );
    let mut editor = pool.begin().await.unwrap();
    sqlx::query("UPDATE schedules SET max_runs = 4 WHERE id = $1")
        .bind(raced.id)
        .execute(&mut *editor)
        .await
        .unwrap();
    let racer = schedules(&pool);
    let claim = tokio::spawn(async move {
        racer
            .claim_due_run(raced.id, now, CATCHUP, &RunLease::unattached())
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity
                 WHERE datname = current_database() AND wait_event_type = 'Lock'
                   AND pid <> pg_backend_pid())",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if waiting {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the claim must reach the edited schedule's row lock");
    editor.commit().await.unwrap();
    assert_eq!(
        claim.await.unwrap().unwrap(),
        DueClaim::MaxRunsReached,
        "the locked re-check reads the committed cap"
    );
    assert_eq!(
        raw_cursors(&pool, raced.id).await,
        before,
        "cursor and watermark"
    );
    let (runs_claimed, runs): (i64, i64) = sqlx::query_as(
        "SELECT runs_claimed, (SELECT COUNT(*) FROM report_runs WHERE schedule_id = $1)
         FROM schedules WHERE id = $1",
    )
    .bind(raced.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((runs_claimed, runs), (4, 0), "nothing spent, nothing run");
    assert_eq!(list_all_due(&store, now).await.0, expected, "spent: gone");
}

#[tokio::test]
async fn claim_deadline_does_not_leak_to_the_pool() {
    let url = common::create_app_database().await;
    let _storage = StorageState::from_pool(common::fixture_pool(&url, 2).await)
        .await
        .expect("boot the app-state database");
    let pool = common::fixture_pool(&url, 2).await;
    let one = common::fixture_pool(&url, 1).await;
    let session_timeout = async || -> String {
        sqlx::query_scalar("SHOW statement_timeout")
            .fetch_one(&one)
            .await
            .unwrap()
    };
    let baseline = session_timeout().await;
    let store = schedules(&one);
    let now = truncate_to_micros(chrono::Utc::now());
    let sq = seed_saved(&pool, 1, "deadline-leak").await;
    let sched = store
        .create_schedule(
            sq,
            1,
            3600,
            None,
            None,
            0,
            now - chrono::Duration::minutes(1),
        )
        .await
        .unwrap();

    let started = store
        .claim_due_run(sched.id, now, CATCHUP, &RunLease::unattached())
        .await
        .unwrap();
    assert!(matches!(started, DueClaim::Started(_)), "{started:?}");
    assert_eq!(session_timeout().await, baseline, "after a commit");

    // The claim moved the cursor past `now`.
    assert_eq!(
        store
            .claim_due_run(sched.id, now, CATCHUP, &RunLease::unattached())
            .await
            .unwrap(),
        DueClaim::NotDue
    );
    assert_eq!(session_timeout().await, baseline, "after a rollback");

    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM saved_queries WHERE id = $1 FOR UPDATE")
        .bind(sq)
        .execute(&mut *lock)
        .await
        .unwrap();
    let later = now + chrono::Duration::hours(2);
    let waited = std::time::Instant::now();
    let expired = tokio::time::timeout(
        3 * CLAIM_WAIT,
        store.claim_due_run(sched.id, later, CATCHUP, &RunLease::unattached()),
    )
    .await
    .expect("the claim ends at its deadline");
    let waited = waited.elapsed();
    lock.rollback().await.unwrap();
    assert!(
        matches!(expired, Err(DueClaimError::Store(StoreError::WaitExpired))),
        "{expired:?}"
    );
    assert!(waited >= CLAIM_WAIT, "{waited:?}");
    assert_eq!(
        session_timeout().await,
        baseline,
        "after a claim that ran out"
    );
}

const SEEK_NOW: &str = "2026-11-10 00:00:00+00";

/// 6000 schedules over 7 keys, a tenth of them disabled and the rest split
/// among spent (at or past the cap), `max_runs = 0`, unlimited and capped
/// with room. Cursors fall from 1500 minutes before [`SEEK_NOW`] to 500
/// after it, three to a minute, so ties are everywhere.
const SEEK_SEED: &str = "
INSERT INTO saved_queries (key_id, name, query, created_at, updated_at)
SELECT 1 + i % 7, 'seek-' || i, 'service=test', now(), now()
FROM generate_series(1, 6000) i;
INSERT INTO schedules (saved_query_id, key_id, interval_secs, max_runs, runs_claimed,
                       enabled, created_at, updated_at, next_fire_at)
SELECT sq.id, sq.key_id, 3600,
       CASE WHEN i % 10 IN (2, 3, 4) THEN 3 WHEN i % 10 = 5 THEN 0
            WHEN i % 10 IN (6, 7) THEN NULL ELSE 5 END,
       CASE WHEN i % 10 IN (2, 3, 4) THEN 3 + i % 2 WHEN i % 10 IN (8, 9) THEN 2 ELSE 0 END,
       i % 10 NOT IN (0, 1), now(), now(),
       timestamptz '2026-11-10 00:00:00+00' + make_interval(mins => (i * 29) % 2000 - 1500)
FROM generate_series(1, 6000) i JOIN saved_queries sq ON sq.name = 'seek-' || i;
ANALYZE schedules;
";

fn assert_ordered_seek(what: &str, plan: &[String], seek: &str) {
    let text = plan.join("\n");
    assert!(
        plan.iter()
            .any(|line| line.contains("Index Scan using schedules_due_idx on schedules")),
        "{what}: no forward Index Scan on schedules_due_idx\n{text}"
    );
    for node in ["Sort", "Seq Scan", "Bitmap"] {
        assert!(!text.contains(node), "{what}: a {node} node\n{text}");
    }
    let cond = plan
        .iter()
        .find_map(|line| line.trim().strip_prefix("Index Cond: "))
        .unwrap_or_else(|| panic!("{what}: no Index Cond\n{text}"));
    assert!(
        cond.contains(seek),
        "{what}: the Index Cond lacks {seek}\n{text}"
    );
}

/// The due listing is an ordered seek on `schedules_due_idx` (ADR-0018
/// amended 2026-11-11). Over 6000 mixed schedules, analyzed, both
/// [`DUE_FIRST_PAGE`] and [`DUE_RESUMED_PAGE`] read the index in
/// `(next_fire_at, id)` order with no Sort node, whether Postgres plans
/// them for the bound values (custom) or once for any value (generic, as a
/// statement cached past five executions is). The resumed page, from a
/// cursor 1500 candidates deep, puts its row comparison in the Index Cond,
/// so it seeks to the cursor instead of reading and dropping the rows
/// before it. Nothing switches a plan type off. The test prints the mix and
/// every plan; `scripts/evidence/issue-444/explain.sh` records that output.
#[sqlx::test]
async fn due_listing_uses_ordered_seek(pool: PgPool) {
    sqlx::raw_sql(SEEK_SEED).execute(&pool).await.unwrap();
    let mix: Vec<(String, bool, i64)> = sqlx::query_as(
        "SELECT CASE WHEN NOT enabled THEN 'disabled'
                     WHEN max_runs IS NULL THEN 'unlimited'
                     WHEN max_runs = 0 THEN 'max_runs = 0'
                     WHEN runs_claimed >= max_runs THEN 'spent'
                     ELSE 'budget left' END,
                next_fire_at <= $1::timestamptz, COUNT(*)
         FROM schedules GROUP BY 1, 2 ORDER BY 1, 2",
    )
    .bind(SEEK_NOW)
    .fetch_all(&pool)
    .await
    .unwrap();
    println!("schedules by budget and due at {SEEK_NOW}:");
    for (kind, due, count) in &mix {
        println!("  {kind:<13} due={due:<5} {count}");
    }
    let (deep_fire, deep_id): (chrono::DateTime<chrono::Utc>, i64) = sqlx::query_as(
        "SELECT next_fire_at, id FROM schedules
         WHERE enabled AND (max_runs IS NULL OR runs_claimed < max_runs)
           AND next_fire_at <= $1::timestamptz
         ORDER BY next_fire_at, id OFFSET 1500 LIMIT 1",
    )
    .bind(SEEK_NOW)
    .fetch_one(&pool)
    .await
    .unwrap();

    // PREPARE and SET belong to one session.
    let mut conn = pool.acquire().await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "PREPARE due_first AS {DUE_FIRST_PAGE};
         PREPARE due_resumed AS {DUE_RESUMED_PAGE};"
    )))
    .execute(&mut *conn)
    .await
    .unwrap();
    let page = DUE_SCHEDULE_PAGE;
    let deep_fire = deep_fire.to_rfc3339();
    let statements = [
        (
            "due_first",
            format!("'{SEEK_NOW}', {page}"),
            "next_fire_at <= ",
        ),
        (
            "due_resumed",
            format!("'{SEEK_NOW}', '{deep_fire}', {deep_id}, {page}"),
            "ROW(next_fire_at, id) > ROW(",
        ),
    ];
    for mode in ["force_custom_plan", "force_generic_plan"] {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SET plan_cache_mode = {mode}")))
            .execute(&mut *conn)
            .await
            .unwrap();
        for (name, args, seek) in &statements {
            let what = format!("{mode}: EXECUTE {name}({args})");
            let plan: Vec<String> = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                "EXPLAIN EXECUTE {name}({args})"
            )))
            .fetch_all(&mut *conn)
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect();
            println!("\n{what}");
            for line in &plan {
                println!("  {line}");
            }
            assert_ordered_seek(&what, &plan, seek);
            // The mode took: a generic plan keeps the parameters.
            assert_eq!(
                plan.iter().any(|line| line.contains("$1")),
                mode == "force_generic_plan",
                "{what}: plan type\n{}",
                plan.join("\n")
            );
        }
    }
}

#[sqlx::test]
async fn list_all_runs_paginated_and_isolated(pool: PgPool) {
    let store = schedules(&pool);
    let sq1 = seed_saved(&pool, 1, "alpha").await;
    let sq2 = seed_saved(&pool, 1, "beta").await;
    let sq3 = seed_saved(&pool, 2, "other-user").await;
    let sched1 = store
        .create_schedule(sq1, 1, 300, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();
    let sched2 = store
        .create_schedule(sq2, 1, 300, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();
    let sched3 = store
        .create_schedule(sq3, 2, 300, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();

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

    let all = store
        .list_all_runs(
            1,
            100,
            0,
            trawl_api::RunsSortKey::default(),
            trawl_api::RunsSortDir::default(),
        )
        .await
        .unwrap();
    assert_eq!(all.len(), 5);
    let names: Vec<&str> = all.iter().map(|(_, name)| name.as_str()).collect();
    assert!(names.contains(&"alpha"));
    assert!(names.contains(&"beta"));
    assert!(!names.contains(&"other-user"));

    assert_eq!(
        store
            .list_all_runs(
                1,
                2,
                0,
                trawl_api::RunsSortKey::default(),
                trawl_api::RunsSortDir::default()
            )
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        store
            .list_all_runs(
                1,
                2,
                2,
                trawl_api::RunsSortKey::default(),
                trawl_api::RunsSortDir::default()
            )
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(store.count_all_runs(1).await.unwrap(), 5);
    assert_eq!(store.count_all_runs(2).await.unwrap(), 1);
}

#[sqlx::test]
async fn runs_stats_counts_by_status(pool: PgPool) {
    let store = schedules(&pool);

    let (total, success, error, timeout, avg) = store.runs_stats(1).await.unwrap();
    assert_eq!((total, success, error, timeout, avg), (0, 0, 0, 0, None));

    let sq_id = seed_saved(&pool, 1, "stats-test").await;
    let sched = store
        .create_schedule(sq_id, 1, 300, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();

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
    assert_eq!(avg, Some(162));

    let (total, ..) = store.runs_stats(2).await.unwrap();
    assert_eq!(total, 0);
}

#[sqlx::test]
async fn delete_old_runs_retention_and_paths(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "retention").await;
    let sched = store
        .create_schedule(sq_id, 1, 300, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();

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

/// A schedule holds one running row at a time, so seed a schedule's
/// running row after its terminal rows.
async fn seed_retention_run(
    pool: &PgPool,
    store: &ScheduleStore,
    (schedule_id, saved_query_id): (i64, i64),
    status: RunStatus,
    result_path: Option<&str>,
    started_at: chrono::DateTime<chrono::Utc>,
) -> i64 {
    let id = seed_run(store, schedule_id, saved_query_id, "q").await;
    if status != RunStatus::Running {
        let error = (status != RunStatus::Success).then_some("failed");
        let outcome = store
            .finish_run(id, status, 10, Some(1), error, None, result_path)
            .await
            .unwrap();
        assert_eq!(outcome, FinishOutcome::Persisted);
    }
    sqlx::query("UPDATE report_runs SET started_at = $2 WHERE id = $1")
        .bind(id)
        .bind(started_at)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn retention_schedule(pool: &PgPool, store: &ScheduleStore, name: &str) -> (i64, i64) {
    let sq_id = seed_saved(pool, 1, name).await;
    let sched = store
        .create_schedule(sq_id, 1, 300, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();
    (sched.id, sq_id)
}

async fn surviving_run_ids(pool: &PgPool) -> Vec<i64> {
    sqlx::query_scalar("SELECT id FROM report_runs ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap()
}

fn sorted(mut paths: Vec<String>) -> Vec<String> {
    paths.sort();
    paths
}

#[sqlx::test]
async fn retention_by_age_spares_running_runs(pool: PgPool) {
    let store = schedules(&pool);
    let s = retention_schedule(&pool, &store, "age").await;
    let now = chrono::Utc::now();
    let old = now - chrono::Duration::days(40);

    seed_retention_run(&pool, &store, s, RunStatus::Success, Some("age/0"), old).await;
    seed_retention_run(&pool, &store, s, RunStatus::Error, None, old).await;
    let young = seed_retention_run(
        &pool,
        &store,
        s,
        RunStatus::Success,
        Some("age/2"),
        now - chrono::Duration::days(1),
    )
    .await;
    let running = seed_retention_run(&pool, &store, s, RunStatus::Running, None, old).await;

    let (deleted, paths) = store.delete_old_runs(30, 1_000).await.unwrap();
    assert_eq!(deleted, 2);
    assert_eq!(paths, vec!["age/0".to_string()]);
    assert_eq!(surviving_run_ids(&pool).await, vec![young, running]);
}

/// Schedule A's running row is the newest (counting it would cost a
/// terminal slot); schedule B's is the oldest (ranking it would delete it).
#[sqlx::test]
async fn retention_by_count_spares_running_runs(pool: PgPool) {
    let store = schedules(&pool);
    let a = retention_schedule(&pool, &store, "count-a").await;
    let b = retention_schedule(&pool, &store, "count-b").await;
    let now = chrono::Utc::now();
    let hours = |h: i64| now - chrono::Duration::hours(h);

    let mut a_terminal = Vec::new();
    let mut b_terminal = Vec::new();
    for h in [5, 4, 3, 2] {
        a_terminal
            .push(seed_retention_run(&pool, &store, a, RunStatus::Success, None, hours(h)).await);
        b_terminal
            .push(seed_retention_run(&pool, &store, b, RunStatus::Success, None, hours(h)).await);
    }
    let a_running = seed_retention_run(&pool, &store, a, RunStatus::Running, None, hours(1)).await;
    let b_running = seed_retention_run(&pool, &store, b, RunStatus::Running, None, hours(9)).await;

    let (deleted, _) = store.delete_old_runs(36_500, 2).await.unwrap();
    assert_eq!(deleted, 4);
    let mut expected = vec![
        a_terminal[2],
        a_terminal[3],
        b_terminal[2],
        b_terminal[3],
        a_running,
        b_running,
    ];
    expected.sort_unstable();
    assert_eq!(surviving_run_ids(&pool).await, expected);
}

#[sqlx::test]
async fn retention_deletes_every_terminal_status(pool: PgPool) {
    let store = schedules(&pool);
    let aged = retention_schedule(&pool, &store, "status-age").await;
    let excess = retention_schedule(&pool, &store, "status-excess").await;
    let now = chrono::Utc::now();
    let old = now - chrono::Duration::days(40);
    let hours = |h: i64| now - chrono::Duration::hours(h);

    seed_retention_run(
        &pool,
        &store,
        aged,
        RunStatus::Success,
        Some("st/a-success"),
        old,
    )
    .await;
    seed_retention_run(&pool, &store, aged, RunStatus::Error, None, old).await;
    seed_retention_run(
        &pool,
        &store,
        aged,
        RunStatus::Timeout,
        Some("st/a-timeout"),
        old,
    )
    .await;
    let aged_keep =
        seed_retention_run(&pool, &store, aged, RunStatus::Success, None, hours(1)).await;
    seed_retention_run(
        &pool,
        &store,
        excess,
        RunStatus::Success,
        Some("st/e-success"),
        hours(4),
    )
    .await;
    seed_retention_run(
        &pool,
        &store,
        excess,
        RunStatus::Error,
        Some("st/e-error"),
        hours(3),
    )
    .await;
    seed_retention_run(&pool, &store, excess, RunStatus::Timeout, None, hours(2)).await;
    let excess_keep = seed_retention_run(
        &pool,
        &store,
        excess,
        RunStatus::Success,
        Some("st/e-keep"),
        hours(1),
    )
    .await;

    let (deleted, paths) = store.delete_old_runs(30, 1_000).await.unwrap();
    assert_eq!(deleted, 3);
    assert_eq!(
        sorted(paths),
        vec!["st/a-success".to_string(), "st/a-timeout".to_string()]
    );

    let (deleted, paths) = store.delete_old_runs(36_500, 1).await.unwrap();
    assert_eq!(deleted, 3);
    assert_eq!(
        sorted(paths),
        vec!["st/e-error".to_string(), "st/e-success".to_string()]
    );
    assert_eq!(surviving_run_ids(&pool).await, vec![aged_keep, excess_keep]);
}

#[sqlx::test]
async fn retention_breaks_started_at_ties_by_id(pool: PgPool) {
    let store = schedules(&pool);
    let s = retention_schedule(&pool, &store, "ties").await;
    let at = chrono::Utc::now() - chrono::Duration::hours(1);

    let mut ids = Vec::new();
    for _ in 0..4 {
        ids.push(seed_retention_run(&pool, &store, s, RunStatus::Success, None, at).await);
    }

    let (deleted, _) = store.delete_old_runs(36_500, 2).await.unwrap();
    assert_eq!(deleted, 2);
    assert_eq!(surviving_run_ids(&pool).await, vec![ids[2], ids[3]]);
}

#[sqlx::test]
async fn retention_keeps_each_schedule_separately(pool: PgPool) {
    let store = schedules(&pool);
    let a = retention_schedule(&pool, &store, "sep-a").await;
    let b = retention_schedule(&pool, &store, "sep-b").await;
    let now = chrono::Utc::now();
    let minutes = |m: i64| now - chrono::Duration::minutes(m);

    // Interleave the two schedules' start times so a global ranking would
    // keep a different set.
    let mut a_ids = Vec::new();
    for m in [50, 30, 10] {
        a_ids
            .push(seed_retention_run(&pool, &store, a, RunStatus::Success, None, minutes(m)).await);
    }
    let mut b_ids = Vec::new();
    for m in [60, 45, 35, 25, 5] {
        b_ids.push(seed_retention_run(&pool, &store, b, RunStatus::Error, None, minutes(m)).await);
    }

    let (deleted, _) = store.delete_old_runs(36_500, 2).await.unwrap();
    assert_eq!(deleted, 4);
    let mut expected = vec![a_ids[1], a_ids[2], b_ids[3], b_ids[4]];
    expected.sort_unstable();
    assert_eq!(surviving_run_ids(&pool).await, expected);
}

#[sqlx::test]
async fn retention_keep_one_beside_a_newer_running_run(pool: PgPool) {
    let store = schedules(&pool);
    let s = retention_schedule(&pool, &store, "keep-one").await;
    let now = chrono::Utc::now();
    let minutes = |m: i64| now - chrono::Duration::minutes(m);

    let mut terminal = Vec::new();
    for m in [30, 20, 10] {
        terminal
            .push(seed_retention_run(&pool, &store, s, RunStatus::Success, None, minutes(m)).await);
    }
    let running = seed_retention_run(&pool, &store, s, RunStatus::Running, None, minutes(1)).await;

    let (deleted, _) = store.delete_old_runs(36_500, 1).await.unwrap();
    assert_eq!(deleted, 2);
    assert_eq!(surviving_run_ids(&pool).await, vec![terminal[2], running]);
}

#[sqlx::test]
async fn retention_at_zero_still_spares_running_runs(pool: PgPool) {
    let store = schedules(&pool);
    let now = chrono::Utc::now();
    let minutes = |m: i64| now - chrono::Duration::minutes(m);

    let a = retention_schedule(&pool, &store, "zero-a").await;
    seed_retention_run(
        &pool,
        &store,
        a,
        RunStatus::Success,
        Some("zero/a"),
        minutes(20),
    )
    .await;
    let a_running =
        seed_retention_run(&pool, &store, a, RunStatus::Running, None, minutes(10)).await;
    let (deleted, paths) = store.delete_old_runs(36_500, 0).await.unwrap();
    assert_eq!((deleted, paths), (1, vec!["zero/a".to_string()]));
    assert_eq!(surviving_run_ids(&pool).await, vec![a_running]);

    let b = retention_schedule(&pool, &store, "zero-b").await;
    seed_retention_run(&pool, &store, b, RunStatus::Timeout, None, minutes(20)).await;
    let b_running =
        seed_retention_run(&pool, &store, b, RunStatus::Running, None, minutes(10)).await;
    let (deleted, _) = store.delete_old_runs(0, 1_000).await.unwrap();
    assert_eq!(deleted, 1);
    assert_eq!(surviving_run_ids(&pool).await, vec![a_running, b_running]);

    let (deleted, paths) = store.delete_old_runs(0, 0).await.unwrap();
    assert_eq!((deleted, paths), (0, Vec::<String>::new()));
    assert_eq!(surviving_run_ids(&pool).await, vec![a_running, b_running]);
}

#[sqlx::test]
async fn retention_cutoff_holds_the_configured_bound(pool: PgPool) {
    let store = schedules(&pool);
    let s = retention_schedule(&pool, &store, "bound").await;
    let run = seed_retention_run(
        &pool,
        &store,
        s,
        RunStatus::Success,
        None,
        chrono::Utc::now() - chrono::Duration::days(400),
    )
    .await;

    let (deleted, _) = store
        .delete_old_runs(trawl_config::MAX_SCHEDULER_REPORT_RETENTION_DAYS, 1)
        .await
        .unwrap();
    assert_eq!(deleted, 0);

    let err = store.delete_old_runs(u64::MAX, 1).await.unwrap_err();
    assert!(matches!(err, StoreError::Validation(_)), "{err:?}");
    assert_eq!(
        surviving_run_ids(&pool).await,
        vec![run],
        "a refusal deletes nothing"
    );
}

#[sqlx::test]
async fn successful_run_selectors(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "sel").await;
    let sched = store
        .create_schedule(sq_id, 1, 300, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();

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
    assert_eq!(
        latest.id, r3,
        "the newest success, whether or not it wrote a file"
    );

    let all = store.list_successful_runs(sq_id).await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, r2);
}

#[sqlx::test]
async fn concurrent_start_run_yields_exactly_one_claim(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "race").await;
    let sched = store
        .create_schedule(sq_id, 1, 300, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();

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

#[sqlx::test]
async fn concurrent_claims_respect_max_runs(pool: PgPool) {
    let store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "cap").await;
    let sched = store
        .create_schedule(sq_id, 1, 300, Some(2), None, 0, chrono::Utc::now())
        .await
        .unwrap();

    // One finished run: exactly one claim slot remains under max_runs = 2.
    let rid = seed_run(&store, sched.id, sq_id, "q").await;
    store
        .finish_run(rid, RunStatus::Success, 10, None, None, None, None)
        .await
        .unwrap();

    let s1 = store.clone();
    let s2 = store.clone();
    let (a, b) = tokio::join!(
        tokio::spawn(async move { s1.claim_run(sched.id, sq_id, "q", None).await }),
        tokio::spawn(async move { s2.claim_run(sched.id, sq_id, "q", None).await }),
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
    assert_eq!(
        runs_claimed(&pool, sched.id).await,
        2,
        "nor does the budget"
    );
}

#[sqlx::test]
async fn finish_run_after_cascade_delete_reports_orphan(pool: PgPool) {
    let saved_store = saved(&pool);
    let store = schedules(&pool);
    let sq = saved_store.create(1, "midflight", "q").await.unwrap();
    let sched = store
        .create_schedule(sq.id, 1, 300, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();

    let rid = seed_run(&store, sched.id, sq.id, "q").await;

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

#[sqlx::test]
async fn fail_run_if_running_is_a_guarded_transition(pool: PgPool) {
    let saved_store = saved(&pool);
    let store = schedules(&pool);

    let sq = saved_store.create(1, "running", "q").await.unwrap();
    let sched = store
        .create_schedule(sq.id, 1, 300, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();
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

    let sq2 = saved_store.create(1, "committed", "q").await.unwrap();
    let sched2 = store
        .create_schedule(sq2.id, 1, 300, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();
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

/// `delete` locks the parent and every run row first, so it either collects
/// the path or `finish_run` reports the orphan: exactly one side owns the
/// cleanup.
#[sqlx::test]
async fn delete_racing_finish_run_never_orphans_path(pool: PgPool) {
    const PATH: &str = "scheduled/race/run.parquet";
    let saved_store = saved(&pool);
    let sched_store = schedules(&pool);
    let sq = saved_store.create(1, "race", "q").await.unwrap();
    let sched = sched_store
        .create_schedule(sq.id, 1, 300, None, None, 0, chrono::Utc::now())
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

/// The same delete against a `since_last` finish, which reaches for the
/// schedule row that the delete's cascade also needs.
///
/// This is a lock CYCLE, not just a race. `SavedQueryStore::delete` ends by
/// deleting the saved query, and that cascade needs the `schedules` row; a
/// `since_last` `finish_run` locks `schedules` first and then updates the
/// run row. With the delete holding the run rows and the finish holding the
/// schedule, each waits on what the other holds and postgres kills one with
/// 40P01. If the victim is the finish, the delete has already read that
/// run's `result_path` as NULL, and the parquet the finish wrote is
/// orphaned with nobody left to unlink it.
///
/// The blocker pins that interleaving by holding the run row: the delete
/// queues for it first, the finish queues behind, and the rollback releases
/// them in that order. The fix is the delete taking the schedule lock at
/// level 2, so both sides walk saved -> schedules -> runs and neither can
/// wait on a lock the other already holds out of order.
#[sqlx::test]
async fn delete_saved_racing_since_last_finish_never_orphans_path(pool: PgPool) {
    const PATH: &str = "scheduled/race/tiled.parquet";
    let saved_store = saved(&pool);
    let sched_store = schedules(&pool);
    let now = truncate_to_micros(chrono::Utc::now());
    let sq = saved_store.create(1, "tiled-race", "q").await.unwrap();
    let sched = sched_store
        .create_schedule(sq.id, 1, 300, None, Some(ScheduleWindow::SinceLast), 0, now)
        .await
        .unwrap();
    let window = ReportWindow {
        start: now - chrono::Duration::seconds(300),
        end: now,
        truncated: false,
        kind: WindowKind::SinceLast,
    };
    let rid = match sched_store
        .claim_run(sched.id, sq.id, "q", Some(&window))
        .await
        .unwrap()
    {
        RunClaim::Started(id) => id,
        other => panic!("expected a started run, got {other:?}"),
    };

    // Hold the run row: both sides end up wanting it, and the delete asks
    // first.
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query_scalar::<_, i64>("SELECT id FROM report_runs WHERE id = $1 FOR UPDATE")
        .bind(rid)
        .fetch_one(&mut *blocker)
        .await
        .unwrap();

    let delete_store = saved_store.clone();
    let sq_id = sq.id;
    let delete_task = tokio::spawn(async move { delete_store.delete(sq_id, 1).await });
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    let finish_store = sched_store.clone();
    let finish_task = tokio::spawn(async move {
        finish_store
            .finish_run(rid, RunStatus::Success, 10, Some(1), None, None, Some(PATH))
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    blocker.rollback().await.unwrap();

    let delete_paths = delete_task
        .await
        .unwrap()
        .unwrap_or_else(|e| panic!("the delete must not be a deadlock victim: {e}"));
    let finished = finish_task
        .await
        .unwrap()
        .unwrap_or_else(|e| panic!("the finish must not be a deadlock victim: {e}"));

    assert_eq!(
        delete_paths.contains(&PATH.to_string()),
        finished == FinishOutcome::Persisted,
        "exactly one side must own the parquet cleanup: delete returned {delete_paths:?}, \
         finish returned {finished:?}"
    );
    assert_eq!(sched_store.count_runs(sched.id).await.unwrap(), 0);
}

#[sqlx::test]
async fn delete_schedule_racing_finish_run_never_orphans_path(pool: PgPool) {
    const PATH: &str = "scheduled/race/sched.parquet";
    let sched_store = schedules(&pool);
    let sq_id = seed_saved(&pool, 1, "race").await;
    let sched = sched_store
        .create_schedule(sq_id, 1, 300, None, None, 0, chrono::Utc::now())
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

/// Boot one `StorageState` on `url` through the real boot path.
async fn boot(url: &str) -> Result<StorageState, StoreError> {
    StorageState::from_pool(common::app_pool(url).await).await
}

#[sqlx::test]
async fn initial_schema_refuses_absurd_intervals(pool: PgPool) {
    let saved_id = saved(&pool)
        .create(1, "duration-boundary", "q")
        .await
        .unwrap()
        .id;
    for interval in [315_360_001_i64, i64::MAX] {
        let error = sqlx::query(
            "INSERT INTO schedules (saved_query_id, key_id, interval_secs, created_at, updated_at, next_fire_at)
             VALUES ($1, 1, $2, now(), now(), now())",
        ).bind(saved_id).bind(interval).execute(&pool).await.unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().constraint(),
            Some("schedules_interval_within_cap")
        );
    }
}

#[tokio::test]
async fn boot_migrates_fresh_database() {
    let url = common::create_app_database().await;
    let storage = boot(&url).await.expect("first boot");

    storage.saved.create(1, "boot", "q").await.unwrap();
    storage.ping().await.unwrap();
}

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

/// A connection merely held checked out would spend a slot for the process
/// lifetime: on a pool of one, the migration that runs right after would
/// wait for a connection that never comes back.
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

    storage.ping().await.unwrap();
    storage.saved.create(1, "detached", "q").await.unwrap();
}

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

#[tokio::test]
async fn lock_loss_is_detected_and_frees_the_lock_for_a_replacement() {
    use std::time::Duration;

    let url = common::create_app_database().await;
    let first = boot(&url).await.expect("first boot");

    let mut lost = first.lock_lost();
    assert!(!*lost.borrow_and_update(), "lock is healthy at boot");
    first.ping_cached().await.expect("storage healthy at boot");

    assert!(
        matches!(boot(&url).await, Err(StoreError::LockHeld)),
        "second live instance must be locked out while the lock is held"
    );

    common::terminate_backends(&url).await;

    // The guard keepalive-probes on an interval, so allow a few cycles.
    tokio::time::timeout(Duration::from_secs(30), lost.wait_for(|v| *v))
        .await
        .expect("guard must detect the lost lock")
        .expect("lock-lost sender must stay alive");

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
        assert_eq!(
            pins.iter().find(|(f, _)| f == "_producer").map(|(_, t)| *t),
            Some(CanonicalType::Varchar)
        );
    }

    #[sqlx::test]
    async fn baseline_restart_preserves_catalog_state_and_sender_pins(pool: PgPool) {
        let initial: (String, bool) =
            sqlx::query_as("SELECT catalog_id::text, conformed_at IS NULL FROM catalog_state")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(initial.0.len(), 36);
        assert!(initial.1);
        let store = catalog(&pool);
        store
            .pin_missing(&[proposal("severity", CanonicalType::BigInt)])
            .await
            .unwrap();
        sqlx::query("UPDATE catalog_state SET conformed_at=now()")
            .execute(&pool)
            .await
            .unwrap();
        let before: String =
            sqlx::query_scalar("SELECT to_jsonb(catalog_state)::text FROM catalog_state")
                .fetch_one(&pool)
                .await
                .unwrap();
        trawl_server::store::migrations::migrate(&pool)
            .await
            .unwrap();
        let after: String =
            sqlx::query_scalar("SELECT to_jsonb(catalog_state)::text FROM catalog_state")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(before, after);
        let pins = store.load_pins().await.unwrap();
        assert_eq!(
            pins.iter().find(|(f, _)| f == "severity").map(|(_, t)| *t),
            Some(CanonicalType::BigInt)
        );
        assert_eq!(pins.len(), ENVELOPE_TYPES.len() + 1);
    }

    #[sqlx::test]
    async fn pin_missing_is_first_writer_wins(pool: PgPool) {
        let store = catalog(&pool);

        let pins = store
            .pin_missing(&[proposal("duration", CanonicalType::BigInt)])
            .await
            .unwrap();
        assert_eq!(pins.get("duration"), Some(&CanonicalType::BigInt));

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
    /// The wide alphabet is load-bearing: Postgres pglz-compresses an
    /// oversized index value before giving up, so a repeated-character
    /// name of the same length slips under the limit.
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

        let all = store.load_pins().await.unwrap();
        assert!(all.iter().all(|(f, _)| f != &huge));
    }

    #[sqlx::test]
    async fn pin_missing_caps_catalog_cardinality(pool: PgPool) {
        let seeded = i64::try_from(ENVELOPE_TYPES.len()).unwrap();
        let store = catalog(&pool).with_pin_cap(seeded + 2);

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

    #[sqlx::test]
    async fn field_services_pages_a_large_history_without_gaps(pool: PgPool) {
        let store = catalog(&pool);

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
            assert_eq!(rows[0].1, vec!["v39".to_owned()], "{field}");
        }
    }

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

        let (degraded, pairs) = store.degraded_snapshot(&visible).await.unwrap();
        assert!(
            degraded.is_empty(),
            "the repinned field is no longer badged"
        );
        assert!(pairs.is_empty(), "and nothing is attributed to it");
    }

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

        let path = rows.iter().find(|r| r.field == "path").unwrap();
        assert_eq!(path.service_count, 0);
        assert_eq!(path.row_count, 0);
        assert_eq!(path.conflict_count, 0);
        assert!(path.first_seen.is_none());
        assert!(path.last_seen.is_none());

        assert!(rows.iter().any(|r| r.field == "_time"));
        assert_eq!(rows.len(), ENVELOPE_TYPES.len() + 2);
    }

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

        let (rows, _) = store
            .list_fields(&trawl_server::store::FieldListFilter::default())
            .await
            .unwrap();
        let duration = rows.iter().find(|r| r.field == "duration").unwrap();
        assert_eq!(duration.conflict_count, 1);
        assert_eq!(duration.rows_nulled, 7);
    }

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

        let (rows, truncated) = store.recent_conflicts(None, None, None, 100).await.unwrap();
        assert_eq!(rows.len(), 3);
        assert!(!truncated);
        assert_eq!(rows[0].field, "path");
        assert_eq!(rows[2].service, "svc-a");
        assert_eq!(rows[2].field, "duration");

        let (rows, _) = store
            .recent_conflicts(Some("duration"), None, None, 100)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.field == "duration"));

        let (rows, _) = store
            .recent_conflicts(None, Some("svc-b"), None, 100)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].service, "svc-b");

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
        store
            .touch_services("svc-b", &["multi".to_owned()], 1)
            .await
            .unwrap();

        age_observation(&pool, "stale", "svc-a", 60).await;
        age_observation(&pool, "edge", "svc-a", 30).await;
        age_observation(&pool, "multi", "svc-a", 60).await;

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

        let stale = candidates.iter().find(|c| c.field == "stale").unwrap();
        assert_eq!(stale.duckdb_type, "VARCHAR");
        assert_eq!(stale.services, 1);
        assert!(stale.last_seen.is_some_and(|seen| seen < cutoff));
    }

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

    /// chrono's minimum is outside `timestamptz`, so binding it fails at the
    /// driver; `catalog::gc::cutoff_for` clamps an unsubtractable window to the epoch.
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

        let pins = store.load_pins().await.unwrap();
        assert!(pins.iter().any(|(f, _)| f == "duration"));
        assert!(pins.iter().any(|(f, _)| f == "_severity"));
    }

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
            sqlx::query(
                "INSERT INTO field_degraded_ack (field, acked_by, note, evidence_through)
                 VALUES ($1, 'key-op', 'seen', 1)",
            )
            .bind(field)
            .execute(&pool)
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
            (
                "field_degraded_ack",
                "SELECT count(*)::bigint FROM field_degraded_ack WHERE field = $1",
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
                    max_nulled_rows: None,
                    max_ambiguous_rows: None,
                    requester: Some(&trawl_server::store::Requester::new(1, "ops")),
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

mod degraded_ack {
    use sqlx::PgPool;
    use sqlx::Row as _;
    use trawl_core::schema::CanonicalType;
    use trawl_server::store::{AckOutcome, CatalogStore, FieldConflict};

    fn catalog(pool: &PgPool) -> CatalogStore {
        CatalogStore::new(pool.clone())
    }

    fn episode(field: &str) -> FieldConflict {
        FieldConflict {
            field: field.to_owned(),
            service: "svc-a".to_owned(),
            observed_type: "VARCHAR".to_owned(),
            expected_type: CanonicalType::BigInt,
            rows_nulled: 3,
            samples: vec!["n/a".to_owned()],
        }
    }

    /// Record `episodes` conflict episodes for a pinned field and age the
    /// evidence past the analyzer's 24h span.
    async fn degrade(pool: &PgPool, field: &str, episodes: usize) {
        let store = catalog(pool);
        for _ in 0..episodes {
            store.record_conflicts(&[episode(field)]).await.unwrap();
        }
        sqlx::query(
            "UPDATE field_conflict_stats SET first_at = now() - interval '48 hours'
             WHERE field = $1",
        )
        .bind(field)
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test]
    async fn ack_upsert_advances_the_high_water_and_replaces_note_and_actor(pool: PgPool) {
        let store = catalog(&pool);
        degrade(&pool, "host", 3).await;

        let AckOutcome::Acked {
            ack,
            created,
            advanced,
        } = store
            .acknowledge_degraded_field("host", "key-aaa", Some("that shipper really sends n/a"))
            .await
            .unwrap()
        else {
            panic!("degraded evidence must be acknowledgeable");
        };
        assert!(created, "the first ack inserts the row");
        assert!(advanced, "a fresh row is this call's own high-water");
        assert_eq!(ack.field, "host");
        assert_eq!(ack.acked_by, "key-aaa");
        assert_eq!(ack.note.as_deref(), Some("that shipper really sends n/a"));
        assert_eq!(ack.evidence_through, 3, "the episode sum at ack time");

        degrade(&pool, "host", 2).await;
        let AckOutcome::Acked {
            ack: again,
            created,
            advanced,
        } = store
            .acknowledge_degraded_field("host", "key-bbb", None)
            .await
            .unwrap()
        else {
            panic!("a second ack must be accepted");
        };
        assert!(!created, "the second ack advances the existing row");
        assert!(advanced, "it saw more evidence than the row carried");
        assert_eq!(again.acked_by, "key-bbb");
        assert_eq!(again.note, None, "an ack without a note clears the old one");
        assert_eq!(again.evidence_through, 5, "the high-water advanced");
        assert!(again.acked_at >= ack.acked_at);

        assert_eq!(store.degraded_ack("host").await.unwrap(), Some(again));
    }

    #[sqlx::test]
    async fn a_stale_ack_leaves_the_higher_water_marks_attribution_alone(pool: PgPool) {
        let store = catalog(&pool);
        degrade(&pool, "host", 6).await;
        let AckOutcome::Acked { ack: first, .. } = store
            .acknowledge_degraded_field("host", "key-aaa", Some("A looked at this"))
            .await
            .unwrap()
        else {
            panic!("degraded evidence must be acknowledgeable");
        };
        assert_eq!(first.evidence_through, 6);

        // The evidence goes away and comes back smaller, which is what a
        // repin's `clear_conflict_evidence` plus a fresh batch of conflicts
        // does.
        sqlx::query("DELETE FROM field_conflict_stats WHERE field = 'host'")
            .execute(&pool)
            .await
            .unwrap();
        degrade(&pool, "host", 3).await;

        let AckOutcome::Acked {
            ack: after,
            created,
            advanced,
        } = store
            .acknowledge_degraded_field("host", "key-bbb", Some("B looked at less"))
            .await
            .unwrap()
        else {
            panic!("the stale ack is still an ordinary answer, not an error");
        };
        assert!(!created);
        assert!(!advanced, "B's read covered less evidence than the row did");
        assert_eq!(after.acked_by, "key-aaa", "A stays the acknowledger");
        assert_eq!(after.note.as_deref(), Some("A looked at this"));
        assert_eq!(after.acked_at, first.acked_at, "the timestamp is A's");
        assert_eq!(after.evidence_through, 6, "the high-water never drops");
        assert_eq!(store.degraded_ack("host").await.unwrap(), Some(after));
    }

    #[sqlx::test]
    async fn ack_refuses_below_the_degraded_threshold(pool: PgPool) {
        let store = catalog(&pool);

        assert_eq!(
            store
                .acknowledge_degraded_field("nonexistent", "key-aaa", None)
                .await
                .unwrap(),
            AckOutcome::Unpinned,
            "no pin, nothing to acknowledge"
        );
        assert_eq!(
            store
                .acknowledge_degraded_field("host", "key-aaa", None)
                .await
                .unwrap(),
            AckOutcome::NotDegraded,
            "a pinned field with no evidence at all"
        );

        // Two episodes over 48 hours: the span is met, the volume is not
        // (three episodes, or a hundred rows shelved).
        degrade(&pool, "host", 2).await;
        assert_eq!(
            store
                .acknowledge_degraded_field("host", "key-aaa", None)
                .await
                .unwrap(),
            AckOutcome::NotDegraded,
            "one episode short of the gate"
        );
        assert_eq!(store.degraded_ack("host").await.unwrap(), None);

        degrade(&pool, "host", 1).await;
        assert!(matches!(
            store
                .acknowledge_degraded_field("host", "key-aaa", None)
                .await
                .unwrap(),
            AckOutcome::Acked { .. }
        ));
    }

    #[sqlx::test]
    async fn clearing_an_ack_reports_whether_a_row_went(pool: PgPool) {
        let store = catalog(&pool);
        degrade(&pool, "host", 3).await;
        store
            .acknowledge_degraded_field("host", "key-aaa", None)
            .await
            .unwrap();

        assert!(store.clear_degraded_ack_owned("host").await.unwrap());
        assert_eq!(store.degraded_ack("host").await.unwrap(), None);
        assert!(
            !store.clear_degraded_ack_owned("host").await.unwrap(),
            "a second clear removes nothing"
        );
    }

    #[sqlx::test]
    async fn deleting_the_pin_cascades_the_ack_away(pool: PgPool) {
        let store = catalog(&pool);
        degrade(&pool, "host", 3).await;
        store
            .acknowledge_degraded_field("host", "key-aaa", None)
            .await
            .unwrap();
        assert!(store.degraded_ack("host").await.unwrap().is_some());

        sqlx::query("DELETE FROM field_types WHERE field = 'host'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            store.degraded_ack("host").await.unwrap(),
            None,
            "the FK cascade removed it"
        );
    }

    #[sqlx::test]
    async fn both_aggregate_forms_report_the_same_acked_through(pool: PgPool) {
        let store = catalog(&pool);
        degrade(&pool, "host", 3).await;
        degrade(&pool, "message", 3).await;
        store
            .acknowledge_degraded_field("host", "key-aaa", None)
            .await
            .unwrap();

        let keyed = store
            .conflict_aggregates(Some(&["host".to_owned(), "message".to_owned()]))
            .await
            .unwrap();
        let all = store.conflict_aggregates(None).await.unwrap();
        let through = |rows: &[trawl_server::catalog::analyzer::ConflictAggregate], f: &str| {
            rows.iter()
                .find(|a| a.field == f)
                .expect("the field's evidence")
                .ack_evidence_through
        };
        assert_eq!(through(&keyed, "host"), Some(3));
        assert_eq!(through(&all, "host"), through(&keyed, "host"));
        assert_eq!(
            through(&keyed, "message"),
            None,
            "an unacknowledged field carries no high-water"
        );
        assert_eq!(through(&all, "message"), None);
        assert_eq!(
            keyed.len(),
            all.len(),
            "the join is LEFT: it must not drop a field"
        );
    }

    #[sqlx::test]
    async fn field_health_snapshot_carries_the_verdict_and_its_ack_together(pool: PgPool) {
        let store = catalog(&pool);
        degrade(&pool, "host", 3).await;

        let health = store.field_health_snapshot("host").await.unwrap();
        let agg = health.aggregate.expect("the field has evidence");
        assert_eq!(agg.episodes, 3);
        assert_eq!(agg.ack_evidence_through, None);
        assert!(health.ack.is_none());
        assert!(
            !health.evidence.is_empty(),
            "the verdict's cited samples come from the same snapshot"
        );

        store
            .acknowledge_degraded_field("host", "key-aaa", Some("known"))
            .await
            .unwrap();

        let health = store.field_health_snapshot("host").await.unwrap();
        let agg = health.aggregate.expect("the evidence is still there");
        let ack = health.ack.expect("the acknowledgement");
        assert_eq!(
            agg.ack_evidence_through,
            Some(ack.evidence_through),
            "the aggregate's high-water and the ack row are one reading"
        );
        assert_eq!(ack.acked_by, "key-aaa");

        let health = store.field_health_snapshot("service").await.unwrap();
        assert!(health.aggregate.is_none());
        assert!(health.evidence.is_empty());
        assert!(health.ack.is_none());
    }

    #[sqlx::test]
    async fn the_note_cap_is_enforced_in_bytes(pool: PgPool) {
        let store = catalog(&pool);
        degrade(&pool, "host", 3).await;

        let at_cap = "n".repeat(1024);
        assert!(matches!(
            store
                .acknowledge_degraded_field("host", "key-aaa", Some(&at_cap))
                .await
                .unwrap(),
            AckOutcome::Acked { .. }
        ));

        let over = "n".repeat(1025);
        store
            .acknowledge_degraded_field("host", "key-aaa", Some(&over))
            .await
            .expect_err("an over-long note is refused");
        let stored: String =
            sqlx::query("SELECT note FROM field_degraded_ack WHERE field = 'host'")
                .fetch_one(&pool)
                .await
                .unwrap()
                .try_get("note")
                .unwrap();
        assert_eq!(stored.len(), 1024, "the refused note did not land");
    }
}

mod repin_store {
    use sqlx::PgPool;
    use trawl_core::schema::CanonicalType;
    use trawl_server::store::{
        CatalogStore, JobTotals, RepinClaim, RepinJobStatus, RepinPlan, RepinStore, Requester,
        StoreError,
    };

    fn store(pool: &PgPool) -> RepinStore {
        RepinStore::new(pool.clone())
    }

    /// Seed the `field_types` row a claim revalidates against.
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

    #[sqlx::test]
    async fn get_requested_by_matches_the_claiming_key_id_only(pool: PgPool) {
        fn claim(requester: Option<&Requester>) -> RepinClaim<'_> {
            RepinClaim {
                field: "status",
                from_type: CanonicalType::BigInt,
                to_type: CanonicalType::Varchar,
                dialect: None,
                dry_run: true,
                force: false,
                requester,
                max_nulled_rows: None,
                max_ambiguous_rows: None,
            }
        }
        let s = store(&pool);
        pin(&pool, "status", CanonicalType::BigInt).await;
        let mut ids = Vec::new();
        // The same display name on both keys: the name must decide nothing.
        for requester in [
            Some(Requester::new(7, "ops")),
            Some(Requester::new(8, "ops")),
            None,
        ] {
            let id = s.claim(claim(requester.as_ref())).await.unwrap();
            assert!(
                s.finish_if_running(id, RepinJobStatus::Succeeded, None, None)
                    .await
                    .unwrap(),
                "free the one-running slot for the next claim"
            );
            ids.push(id);
        }
        let [own, other, internal] = ids[..] else {
            unreachable!("three claims")
        };

        let row = s.get(own).await.unwrap().expect("the claimed row");
        assert_eq!(row.requested_by.as_deref(), Some("ops"));
        assert_eq!(row.requested_by_key_id, Some(7));
        let row = s.get(internal).await.unwrap().expect("the internal row");
        assert_eq!(row.requested_by, None);
        assert_eq!(row.requested_by_key_id, None);

        let mine = s.get_requested_by(own, 7).await.unwrap().expect("own job");
        assert_eq!(mine.id, own);
        assert_eq!(mine.requested_by_key_id, Some(7));
        for (what, id) in [
            ("another key's job", other),
            ("a job no key requested", internal),
            ("an id never issued", internal + 1000),
        ] {
            assert!(
                s.get_requested_by(id, 7).await.unwrap().is_none(),
                "{what} must read as absent to key 7"
            );
        }
        assert!(s.get(other).await.unwrap().is_some());
    }

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
                requester: Some(&Requester::new(1, "key-1")),
                max_nulled_rows: None,
                max_ambiguous_rows: None,
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
                requester: None,
                max_nulled_rows: None,
                max_ambiguous_rows: None,
            })
            .await
            .expect_err("a second running job must refuse");
        assert!(matches!(err, StoreError::RepinAlreadyRunning));

        assert!(
            s.finish_if_running(id, RepinJobStatus::Failed, Some("test"), None)
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
            requester: None,
            max_nulled_rows: None,
            max_ambiguous_rows: None,
        })
        .await
        .expect("a terminal job frees the one-running slot");
    }

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
                requester: None,
                max_nulled_rows: None,
                max_ambiguous_rows: None,
            }),
            s2.claim(RepinClaim {
                field: "status",
                from_type: CanonicalType::BigInt,
                to_type: CanonicalType::Varchar,
                dialect: None,
                dry_run: false,
                force: false,
                requester: None,
                max_nulled_rows: None,
                max_ambiguous_rows: None,
            }),
        );
        let wins = [&a, &b].iter().filter(|r| r.is_ok()).count();
        assert_eq!(wins, 1, "exactly one claim may win: {a:?} / {b:?}");
        let loser = if a.is_err() { a } else { b };
        assert!(matches!(loser, Err(StoreError::RepinAlreadyRunning)));
    }

    #[sqlx::test]
    async fn finish_cutover_flips_pin_and_job_transactionally_and_idempotently(pool: PgPool) {
        let s = store(&pool);
        let catalog = CatalogStore::new(pool.clone());
        // `host` is seeded VARCHAR by the initial schema; the flip retypes it.
        let id = s
            .claim(RepinClaim {
                field: "host",
                from_type: CanonicalType::Varchar,
                to_type: CanonicalType::BigInt,
                dialect: None,
                dry_run: false,
                force: false,
                requester: None,
                max_nulled_rows: None,
                max_ambiguous_rows: None,
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

        s.finish_cutover(id, "host", CanonicalType::BigInt)
            .await
            .unwrap();
        let again = s.get(id).await.unwrap().unwrap();
        assert_eq!(again.status, RepinJobStatus::Succeeded);
        assert_eq!(again.finished_at, job.finished_at, "redo must not restamp");
    }

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
                requester: None,
                max_nulled_rows: None,
                max_ambiguous_rows: None,
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
                requester: None,
                max_nulled_rows: None,
                max_ambiguous_rows: None,
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

    #[sqlx::test]
    async fn finish_cutover_materialises_the_staged_loss(pool: PgPool) {
        let s = store(&pool);
        let catalog = CatalogStore::new(pool.clone());
        let id = running_execution(&pool, "duration").await;
        s.stage_cutover_input(
            id,
            JobTotals {
                files_done: 2,
                rows_rewritten: 100,
                rows_nulled: 9,
                rows_resurrected: 0,
                ambiguous_numerals: 0,
            },
            &[("nginx".to_owned(), 7), ("api".to_owned(), 2)],
        )
        .await
        .unwrap();

        s.finish_cutover(id, "duration", CanonicalType::BigInt)
            .await
            .unwrap();

        let job = s.get(id).await.unwrap().expect("job row");
        assert_eq!(job.status, RepinJobStatus::Succeeded);

        let mut conflicts = catalog.conflicts_for_field("duration").await.unwrap();
        conflicts.sort_by(|a, b| a.service.cmp(&b.service));
        assert_eq!(
            conflicts.len(),
            2,
            "one row per lossy service: {conflicts:?}"
        );
        assert_eq!(conflicts[0].service, "api");
        assert_eq!(conflicts[0].rows_nulled, 2);
        assert_eq!(conflicts[1].service, "nginx");
        assert_eq!(conflicts[1].rows_nulled, 7);
        for row in &conflicts {
            assert_eq!(
                row.observed_type, "VARCHAR",
                "evidence names the pin the values were stored under"
            );
            assert_eq!(row.expected_type, "BIGINT");
            assert!(
                row.samples.is_empty(),
                "the rewrite counts nulls, it never carries the values back"
            );
        }

        let aggregates = catalog
            .conflict_aggregates(Some(&["duration".to_owned()]))
            .await
            .unwrap();
        assert_eq!(aggregates.len(), 1);
        assert_eq!(aggregates[0].episodes, 2);
        assert_eq!(aggregates[0].rows_nulled_total, 9);
    }

    #[sqlx::test]
    async fn finish_cutover_replay_does_not_double_insert_staged_evidence(pool: PgPool) {
        let s = store(&pool);
        let catalog = CatalogStore::new(pool.clone());
        let id = running_execution(&pool, "duration").await;
        s.stage_cutover_input(
            id,
            JobTotals {
                rows_nulled: 4,
                ..JobTotals::default()
            },
            &[("nginx".to_owned(), 4)],
        )
        .await
        .unwrap();

        for _ in 0..3 {
            s.finish_cutover(id, "duration", CanonicalType::BigInt)
                .await
                .unwrap();
        }

        assert_eq!(
            catalog.conflicts_for_field("duration").await.unwrap().len(),
            1
        );
        let aggregates = catalog
            .conflict_aggregates(Some(&["duration".to_owned()]))
            .await
            .unwrap();
        assert_eq!(aggregates[0].episodes, 1, "one episode, replayed twice");
        assert_eq!(aggregates[0].rows_nulled_total, 4);
    }

    #[sqlx::test]
    async fn finish_cutover_records_nothing_for_a_lossless_repin(pool: PgPool) {
        let s = store(&pool);
        let catalog = CatalogStore::new(pool.clone());
        let id = running_execution(&pool, "duration").await;
        s.stage_cutover_input(id, JobTotals::default(), &[])
            .await
            .unwrap();

        s.finish_cutover(id, "duration", CanonicalType::BigInt)
            .await
            .unwrap();

        assert_eq!(
            s.get(id).await.unwrap().unwrap().status,
            RepinJobStatus::Succeeded
        );
        assert!(
            catalog
                .conflicts_for_field("duration")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[sqlx::test]
    async fn finish_cutover_completes_forward_when_nothing_was_staged(pool: PgPool) {
        let s = store(&pool);
        let catalog = CatalogStore::new(pool.clone());
        let id = running_execution(&pool, "duration").await;
        assert_eq!(
            staged(&pool, id).await,
            (None, None),
            "the premise: this job never staged"
        );

        s.finish_cutover(id, "duration", CanonicalType::BigInt)
            .await
            .expect("an unstaged completing call goes forward, never refuses");

        assert_eq!(
            s.get(id).await.unwrap().unwrap().status,
            RepinJobStatus::Succeeded
        );
        assert!(
            catalog
                .conflicts_for_field("duration")
                .await
                .unwrap()
                .is_empty(),
            "no tallies means no evidence, never invented evidence"
        );
    }

    #[sqlx::test]
    async fn finish_cutover_refuses_a_flip_the_job_never_claimed(pool: PgPool) {
        let s = store(&pool);
        let catalog = CatalogStore::new(pool.clone());
        let id = running_execution(&pool, "duration").await;

        let err = s
            .finish_cutover(id, "status", CanonicalType::BigInt)
            .await
            .expect_err("a different field must not complete this job");
        assert!(matches!(err, StoreError::Validation(_)), "{err:?}");

        let err = s
            .finish_cutover(id, "duration", CanonicalType::Varchar)
            .await
            .expect_err("a different target must not complete this job");
        assert!(matches!(err, StoreError::Validation(_)), "{err:?}");

        assert_eq!(
            s.get(id).await.unwrap().unwrap().status,
            RepinJobStatus::Running,
            "a refused cutover leaves the job running"
        );
        let pins: std::collections::HashMap<_, _> =
            catalog.load_pins().await.unwrap().into_iter().collect();
        assert_eq!(
            pins.get("duration"),
            Some(&CanonicalType::Varchar),
            "and the pin the claim was prepared against stands as it was"
        );

        let err = s
            .finish_cutover(id + 10_000, "duration", CanonicalType::BigInt)
            .await
            .expect_err("no such job");
        assert!(matches!(err, StoreError::NotFound { .. }), "{err:?}");
    }

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
                requester: None,
                max_nulled_rows: None,
                max_ambiguous_rows: None,
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
                requester: None,
                max_nulled_rows: None,
                max_ambiguous_rows: None,
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
                accepted_max_nulled_rows: None,
                accepted_max_ambiguous_rows: None,
            },
        )
        .await
        .unwrap();
        assert!(
            s.finish_if_running(first, RepinJobStatus::Succeeded, None, None)
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
                requester: Some(&Requester::new(9, "key-9")),
                max_nulled_rows: None,
                max_ambiguous_rows: None,
            })
            .await
            .unwrap();
        s.record_progress(
            second,
            JobTotals {
                files_done: 5,
                rows_rewritten: 1200,
                rows_nulled: 2,
                rows_resurrected: 7,
                ambiguous_numerals: 3,
            },
        )
        .await
        .unwrap();

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
            s.finish_if_running(
                second,
                RepinJobStatus::Blocked,
                Some("cutover starved"),
                None
            )
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
        assert_eq!(dry.dialect, None);
    }

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
            requester: None,
            max_nulled_rows: None,
            max_ambiguous_rows: None,
        };

        let id = s
            .claim(claim(CanonicalType::Severity, Some(Dialect::Syslog), true))
            .await
            .expect("SEVERITY is a claimable target");
        let job = s.get(id).await.unwrap().unwrap();
        assert_eq!(job.to_type, "SEVERITY");
        assert_eq!(job.dialect.as_deref(), Some("syslog"));
        assert!(
            s.finish_if_running(id, RepinJobStatus::Succeeded, None, None)
                .await
                .unwrap()
        );

        pin(&pool, "level", CanonicalType::Severity).await;
        let back = s
            .claim(RepinClaim {
                field: "level",
                from_type: CanonicalType::Severity,
                to_type: CanonicalType::Varchar,
                dialect: None,
                dry_run: true,
                force: false,
                requester: None,
                max_nulled_rows: None,
                max_ambiguous_rows: None,
            })
            .await
            .expect("a repin away from SEVERITY is claimable");
        let job = s.get(back).await.unwrap().unwrap();
        assert_eq!(job.from_type, "SEVERITY");
        assert_eq!(job.dialect, None);
        assert!(
            s.finish_if_running(back, RepinJobStatus::Succeeded, None, None)
                .await
                .unwrap()
        );

        pin(&pool, "level", CanonicalType::Varchar).await;
        s.claim(claim(CanonicalType::Severity, None, true))
            .await
            .expect_err("a SEVERITY job must carry a dialect");
        s.claim(claim(CanonicalType::BigInt, Some(Dialect::Otel), true))
            .await
            .expect_err("only a SEVERITY job may carry a dialect");
    }

    #[sqlx::test]
    async fn force_ceilings_round_trip_through_claim_and_plan(pool: PgPool) {
        let s = store(&pool);
        pin(&pool, "status", CanonicalType::BigInt).await;
        let id = s
            .claim(RepinClaim {
                field: "status",
                from_type: CanonicalType::BigInt,
                to_type: CanonicalType::Varchar,
                dialect: None,
                dry_run: false,
                force: true,
                max_nulled_rows: Some(40),
                max_ambiguous_rows: Some(0),
                requester: Some(&Requester::new(1, "key-1")),
            })
            .await
            .unwrap();

        let claimed = s.get(id).await.unwrap().unwrap();
        assert_eq!(claimed.max_nulled_rows, Some(40));
        assert_eq!(claimed.max_ambiguous_rows, Some(0));
        assert_eq!(
            (
                claimed.accepted_max_nulled_rows,
                claimed.accepted_max_ambiguous_rows
            ),
            (None, None),
            "nothing is accepted until the scan resolves it"
        );

        s.record_plan(
            id,
            RepinPlan {
                projected_nulls: 30,
                accepted_max_nulled_rows: Some(40),
                accepted_max_ambiguous_rows: Some(0),
                ..RepinPlan::default()
            },
        )
        .await
        .unwrap();

        let planned = s.get(id).await.unwrap().unwrap();
        assert_eq!(planned.max_nulled_rows, Some(40), "the request's echo");
        assert_eq!(planned.max_ambiguous_rows, Some(0));
        assert_eq!(planned.accepted_max_nulled_rows, Some(40));
        assert_eq!(planned.accepted_max_ambiguous_rows, Some(0));
        assert!(planned.planned_at.is_some());
    }

    #[sqlx::test]
    async fn a_forced_plan_without_recorded_bounds_is_rejected(pool: PgPool) {
        let s = store(&pool);
        pin(&pool, "status", CanonicalType::BigInt).await;
        let id = s
            .claim(RepinClaim {
                field: "status",
                from_type: CanonicalType::BigInt,
                to_type: CanonicalType::Varchar,
                dialect: None,
                dry_run: true,
                force: true,
                max_nulled_rows: None,
                max_ambiguous_rows: None,
                requester: None,
            })
            .await
            .unwrap();
        s.record_plan(id, RepinPlan::default())
            .await
            .expect_err("forced plan needs resolved bounds");

        let job = s.get(id).await.unwrap().unwrap();
        assert!(job.planned_at.is_none(), "failed plan write must be atomic");
        assert_eq!(
            (
                job.max_nulled_rows,
                job.max_ambiguous_rows,
                job.accepted_max_nulled_rows,
                job.accepted_max_ambiguous_rows,
            ),
            (None, None, None, None)
        );
    }

    #[sqlx::test]
    async fn force_plan_shape_constraints_preserve_the_unplanned_claim(pool: PgPool) {
        let s = store(&pool);
        pin(&pool, "status", CanonicalType::BigInt).await;
        let id = s
            .claim(RepinClaim {
                field: "status",
                from_type: CanonicalType::BigInt,
                to_type: CanonicalType::Varchar,
                dialect: None,
                dry_run: true,
                force: true,
                max_nulled_rows: None,
                max_ambiguous_rows: None,
                requester: None,
            })
            .await
            .unwrap();
        for (force, planned, nulled, ambiguous, requested) in [
            (true, true, None, None, None),
            (true, true, Some(0_i64), None, None),
            (true, true, None, Some(0_i64), None),
            (true, false, Some(0), Some(0), None),
            (false, true, Some(0), Some(0), None),
            (false, false, None, None, Some(0_i64)),
        ] {
            let error = sqlx::query(
                "UPDATE repin_jobs SET force = $2,
                planned_at = CASE WHEN $3 THEN now() ELSE NULL END,
                accepted_max_nulled_rows = $4, accepted_max_ambiguous_rows = $5,
                max_nulled_rows = $6 WHERE id = $1",
            )
            .bind(id)
            .bind(force)
            .bind(planned)
            .bind(nulled)
            .bind(ambiguous)
            .bind(requested)
            .execute(&pool)
            .await
            .expect_err("invalid force shape must be rejected");
            assert_eq!(
                error.as_database_error().unwrap().code().as_deref(),
                Some("23514")
            );
            let job = s.get(id).await.unwrap().unwrap();
            assert!(job.force);
            assert!(job.planned_at.is_none());
            assert!(job.accepted_max_nulled_rows.is_none());
            assert!(job.accepted_max_ambiguous_rows.is_none());
        }
    }

    #[sqlx::test]
    async fn inconsistent_force_rows_fail_to_decode(pool: PgPool) {
        let s = store(&pool);
        pin(&pool, "status", CanonicalType::BigInt).await;
        let id = s
            .claim(RepinClaim {
                field: "status",
                from_type: CanonicalType::BigInt,
                to_type: CanonicalType::Varchar,
                dialect: None,
                dry_run: true,
                force: true,
                max_nulled_rows: None,
                max_ambiguous_rows: None,
                requester: None,
            })
            .await
            .unwrap();
        sqlx::raw_sql(
            "ALTER TABLE repin_jobs
            DROP CONSTRAINT repin_jobs_accepted_nulled_plan,
            DROP CONSTRAINT repin_jobs_accepted_ambiguous_plan,
            DROP CONSTRAINT repin_jobs_requested_force",
        )
        .execute(&pool)
        .await
        .unwrap();
        for (force, planned, nulled, ambiguous, requested) in [
            (true, true, None, None, None),
            (true, true, Some(0_i64), None, None),
            (true, true, None, Some(0_i64), None),
            (true, false, Some(0), Some(0), None),
            (false, true, Some(0), Some(0), None),
            (false, false, None, None, Some(0_i64)),
        ] {
            sqlx::query(
                "UPDATE repin_jobs SET force = $2,
                planned_at = CASE WHEN $3 THEN now() ELSE NULL END,
                accepted_max_nulled_rows = $4, accepted_max_ambiguous_rows = $5,
                max_nulled_rows = $6 WHERE id = $1",
            )
            .bind(id)
            .bind(force)
            .bind(planned)
            .bind(nulled)
            .bind(ambiguous)
            .bind(requested)
            .execute(&pool)
            .await
            .unwrap();
            s.get(id)
                .await
                .expect_err("inconsistent force terms must not render");
            s.latest()
                .await
                .expect_err("status must also reject inconsistent terms");
        }
    }

    #[sqlx::test]
    async fn finish_cutover_clears_the_degraded_ack(pool: PgPool) {
        let s = store(&pool);
        let catalog = CatalogStore::new(pool.clone());
        for _ in 0..3 {
            catalog
                .record_conflicts(&[trawl_server::store::FieldConflict {
                    field: "host".to_owned(),
                    service: "svc-a".to_owned(),
                    observed_type: "VARCHAR".to_owned(),
                    expected_type: CanonicalType::BigInt,
                    rows_nulled: 3,
                    samples: vec!["n/a".to_owned()],
                }])
                .await
                .unwrap();
        }
        sqlx::query(
            "UPDATE field_conflict_stats SET first_at = now() - interval '48 hours'
             WHERE field = 'host'",
        )
        .execute(&pool)
        .await
        .unwrap();
        catalog
            .acknowledge_degraded_field("host", "key-aaa", Some("known"))
            .await
            .unwrap();

        let id = s
            .claim(RepinClaim {
                field: "host",
                from_type: CanonicalType::Varchar,
                to_type: CanonicalType::BigInt,
                dialect: None,
                dry_run: false,
                force: false,
                max_nulled_rows: None,
                max_ambiguous_rows: None,
                requester: None,
            })
            .await
            .unwrap();

        let outcome = s
            .finish_cutover(id, "host", CanonicalType::BigInt)
            .await
            .unwrap();
        assert!(outcome.completed, "this call completed the job");
        assert!(outcome.cleared_ack, "and removed the acknowledgement");
        assert_eq!(catalog.degraded_ack("host").await.unwrap(), None);

        let replay = s
            .finish_cutover(id, "host", CanonicalType::BigInt)
            .await
            .unwrap();
        assert!(!replay.completed, "the replay completed nothing");
        assert!(!replay.cleared_ack, "so the audit event fires exactly once");
    }

    #[sqlx::test]
    async fn finish_cutover_replay_keeps_an_ack_written_after_the_flip(pool: PgPool) {
        let s = store(&pool);
        let catalog = CatalogStore::new(pool.clone());
        let id = s
            .claim(RepinClaim {
                field: "host",
                from_type: CanonicalType::Varchar,
                to_type: CanonicalType::BigInt,
                dialect: None,
                dry_run: false,
                force: true,
                max_nulled_rows: None,
                max_ambiguous_rows: None,
                requester: None,
            })
            .await
            .unwrap();
        s.finish_cutover(id, "host", CanonicalType::BigInt)
            .await
            .unwrap();

        for _ in 0..3 {
            catalog
                .record_conflicts(&[trawl_server::store::FieldConflict {
                    field: "host".to_owned(),
                    service: "svc-a".to_owned(),
                    observed_type: "VARCHAR".to_owned(),
                    expected_type: CanonicalType::BigInt,
                    rows_nulled: 3,
                    samples: Vec::new(),
                }])
                .await
                .unwrap();
        }
        sqlx::query(
            "UPDATE field_conflict_stats SET first_at = now() - interval '48 hours'
             WHERE field = 'host'",
        )
        .execute(&pool)
        .await
        .unwrap();
        catalog
            .acknowledge_degraded_field("host", "key-aaa", None)
            .await
            .unwrap();

        let replay = s
            .finish_cutover(id, "host", CanonicalType::BigInt)
            .await
            .unwrap();
        assert!(!replay.completed);
        assert!(!replay.cleared_ack);
        assert!(
            catalog.degraded_ack("host").await.unwrap().is_some(),
            "the replay must not touch an ack written after the flip"
        );
    }

    #[sqlx::test]
    async fn a_terminal_write_can_persist_the_counts_behind_its_verdict(pool: PgPool) {
        let s = store(&pool);
        pin(&pool, "dur", CanonicalType::Varchar).await;
        let claim = RepinClaim {
            field: "dur",
            from_type: CanonicalType::Varchar,
            to_type: CanonicalType::BigInt,
            dialect: None,
            dry_run: false,
            force: true,
            max_nulled_rows: None,
            max_ambiguous_rows: None,
            requester: None,
        };
        let id = s.claim(claim).await.unwrap();

        assert_eq!(s.get(id).await.unwrap().unwrap().rows_nulled, 0);

        assert!(
            s.finish_if_running(
                id,
                RepinJobStatus::RefusedNeedsForce,
                Some("11 nulled row(s) over an accepted 10"),
                Some(JobTotals {
                    files_done: 6,
                    rows_rewritten: 17,
                    rows_nulled: 11,
                    rows_resurrected: 2,
                    ambiguous_numerals: 4,
                }),
            )
            .await
            .unwrap()
        );

        let job = s.get(id).await.unwrap().unwrap();
        assert_eq!(job.status, RepinJobStatus::RefusedNeedsForce);
        assert_eq!(
            (
                job.files_done,
                job.rows_rewritten,
                job.rows_nulled,
                job.rows_resurrected,
                job.ambiguous_numerals
            ),
            (6, 17, 11, 2, 4),
            "the refusal and the numbers behind it land in one statement"
        );

        assert!(
            !s.finish_if_running(id, RepinJobStatus::Failed, Some("later"), None)
                .await
                .unwrap()
        );
        let job = s.get(id).await.unwrap().unwrap();
        assert_eq!(job.status, RepinJobStatus::RefusedNeedsForce);
        assert_eq!((job.rows_nulled, job.ambiguous_numerals), (11, 4));

        let second = s.claim(claim).await.unwrap();
        s.record_progress(
            second,
            JobTotals {
                files_done: 1,
                rows_rewritten: 3,
                rows_nulled: 0,
                rows_resurrected: 0,
                ambiguous_numerals: 0,
            },
        )
        .await
        .unwrap();
        assert!(
            s.finish_if_running(second, RepinJobStatus::Failed, Some("later"), None)
                .await
                .unwrap()
        );
        let job = s.get(second).await.unwrap().unwrap();
        assert_eq!(job.status, RepinJobStatus::Failed);
        assert_eq!((job.files_done, job.rows_rewritten), (1, 3));
    }

    /// `acknowledge_degraded_field` serializes against the cutover by taking
    /// `FOR SHARE` on the pin row. The blocker holds exactly the lock the ack takes.
    #[sqlx::test]
    async fn same_type_cutover_waits_on_a_racing_ack(pool: PgPool) {
        let s = store(&pool);
        let catalog = CatalogStore::new(pool.clone());
        for _ in 0..3 {
            catalog
                .record_conflicts(&[trawl_server::store::FieldConflict {
                    field: "host".to_owned(),
                    service: "svc-a".to_owned(),
                    observed_type: "VARCHAR".to_owned(),
                    expected_type: CanonicalType::Varchar,
                    rows_nulled: 3,
                    samples: Vec::new(),
                }])
                .await
                .unwrap();
        }
        sqlx::query(
            "UPDATE field_conflict_stats SET first_at = now() - interval '48 hours'
             WHERE field = 'host'",
        )
        .execute(&pool)
        .await
        .unwrap();
        catalog
            .acknowledge_degraded_field("host", "key-aaa", None)
            .await
            .unwrap();

        let id = s
            .claim(RepinClaim {
                field: "host",
                from_type: CanonicalType::Varchar,
                to_type: CanonicalType::Varchar,
                dialect: None,
                dry_run: false,
                force: true,
                max_nulled_rows: None,
                max_ambiguous_rows: None,
                requester: None,
            })
            .await
            .unwrap();

        let mut blocker = pool.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM field_types WHERE field = 'host' FOR SHARE")
            .fetch_optional(&mut *blocker)
            .await
            .unwrap();

        let cutover_store = store(&pool);
        let mut cutover = tokio::spawn(async move {
            cutover_store
                .finish_cutover(id, "host", CanonicalType::Varchar)
                .await
        });

        let stalled = tokio::time::timeout(std::time::Duration::from_millis(500), &mut cutover)
            .await
            .is_err();
        assert!(
            stalled,
            "the cutover must wait for the ack's row lock even when the type does not change"
        );

        blocker.rollback().await.unwrap();
        let outcome = cutover.await.unwrap().unwrap();
        assert!(outcome.completed);
        assert!(
            outcome.cleared_ack,
            "the cutover that ran second removes the ack the racing operator wrote"
        );
        assert_eq!(catalog.degraded_ack("host").await.unwrap(), None);

        assert_eq!(
            catalog
                .acknowledge_degraded_field("host", "key-aaa", None)
                .await
                .unwrap(),
            trawl_server::store::AckOutcome::NotDegraded
        );
    }

    #[sqlx::test]
    async fn claim_refuses_when_the_from_pin_vanished(pool: PgPool) {
        let s = store(&pool);
        let catalog = CatalogStore::new(pool.clone());
        pin(&pool, "dur", CanonicalType::BigInt).await;
        catalog
            .delete_pins(&["dur".to_owned()])
            .await
            .expect("gc reclaims the slot");

        let ops = Requester::new(1, "ops");
        let taken = |from| RepinClaim {
            field: "dur",
            from_type: from,
            to_type: CanonicalType::Varchar,
            dialect: None,
            dry_run: false,
            force: false,
            max_nulled_rows: None,
            max_ambiguous_rows: None,
            requester: Some(&ops),
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

        s.claim(taken(CanonicalType::Varchar))
            .await
            .expect("the current pin is claimable");
    }

    #[sqlx::test]
    async fn clear_conformed_rearms_the_boot_pass(pool: PgPool) {
        let catalog = CatalogStore::new(pool.clone());
        catalog.mark_conformed().await.unwrap();
        assert!(catalog.is_conformed().await.unwrap());
        catalog.clear_conformed().await.unwrap();
        assert!(!catalog.is_conformed().await.unwrap());
    }

    async fn claim_running(pool: &PgPool, s: &RepinStore) -> i64 {
        pin(pool, "status", CanonicalType::BigInt).await;
        s.claim(RepinClaim {
            field: "status",
            from_type: CanonicalType::BigInt,
            to_type: CanonicalType::Varchar,
            dialect: None,
            dry_run: false,
            force: false,
            max_nulled_rows: None,
            max_ambiguous_rows: None,
            requester: Some(&Requester::new(1, "key-1")),
        })
        .await
        .unwrap()
    }

    #[sqlx::test]
    async fn a_cancelled_job_keeps_its_request_fields(pool: PgPool) {
        let s = store(&pool);
        let id = claim_running(&pool, &s).await;

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
                None,
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

    #[sqlx::test]
    async fn a_second_cancel_request_preserves_the_first(pool: PgPool) {
        let s = store(&pool);
        let id = claim_running(&pool, &s).await;

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
            s.finish_if_running(id, RepinJobStatus::Cancelled, Some("cancelled"), None)
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

    #[sqlx::test]
    async fn finish_if_running_never_overwrites_a_terminal_row(pool: PgPool) {
        let s = store(&pool);
        let id = claim_running(&pool, &s).await;

        // The request has to be on the row before the verdict: the
        // migration refuses a `cancelled` status with no recorded asker.
        s.record_cancel_request(id, "key-op")
            .await
            .unwrap()
            .unwrap();
        assert!(
            s.finish_if_running(
                id,
                RepinJobStatus::Cancelled,
                Some("cancelled by key-op"),
                None
            )
            .await
            .unwrap()
        );
        let after_first = s.get(id).await.unwrap().unwrap();

        assert!(
            !s.finish_if_running(id, RepinJobStatus::Succeeded, None, None)
                .await
                .unwrap(),
            "the row is no longer running"
        );
        let after_second = s.get(id).await.unwrap().unwrap();
        assert_eq!(after_second.status, RepinJobStatus::Cancelled);
        assert_eq!(after_second.error.as_deref(), Some("cancelled by key-op"));
        assert_eq!(after_second.finished_at, after_first.finished_at);
    }

    #[sqlx::test]
    async fn a_failed_job_may_carry_cancel_request_fields(pool: PgPool) {
        let s = store(&pool);
        let id = claim_running(&pool, &s).await;
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

    #[sqlx::test]
    async fn cancelled_without_a_request_is_refused(pool: PgPool) {
        let s = store(&pool);
        let id = claim_running(&pool, &s).await;

        let err = sqlx::query("UPDATE repin_jobs SET status = 'cancelled' WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .expect_err("the CHECK refuses an unrequested cancellation");
        assert!(
            format!("{err}").contains("repin_jobs_cancelled_request_check"),
            "unexpected error: {err}"
        );

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

    /// Read the staged tally columns straight off the row: they are
    /// deliberately absent from `JOB_COLS`.
    async fn staged(pool: &PgPool, id: i64) -> (Option<Vec<String>>, Option<Vec<i64>>) {
        sqlx::query_as("SELECT nulled_services, nulled_service_rows FROM repin_jobs WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn running_execution(pool: &PgPool, field: &str) -> i64 {
        pin(pool, field, CanonicalType::Varchar).await;
        store(pool)
            .claim(RepinClaim {
                field,
                from_type: CanonicalType::Varchar,
                to_type: CanonicalType::BigInt,
                dialect: None,
                dry_run: false,
                force: true,
                max_nulled_rows: None,
                max_ambiguous_rows: None,
                requester: None,
            })
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn stage_cutover_input_round_trips_totals_and_tallies(pool: PgPool) {
        let s = store(&pool);
        let id = running_execution(&pool, "duration").await;

        assert_eq!(
            staged(&pool, id).await,
            (None, None),
            "a claimed job has staged nothing: no column default may say otherwise"
        );

        s.stage_cutover_input(
            id,
            JobTotals {
                files_done: 9,
                rows_rewritten: 4200,
                rows_nulled: 31,
                rows_resurrected: 6,
                ambiguous_numerals: 2,
            },
            &[("nginx".to_owned(), 25), ("api".to_owned(), 6)],
        )
        .await
        .unwrap();

        let job = s.get(id).await.unwrap().expect("job row");
        assert_eq!(job.files_done, 9);
        assert_eq!(job.rows_rewritten, 4200);
        assert_eq!(job.rows_nulled, 31);
        assert_eq!(job.rows_resurrected, 6);
        assert_eq!(job.ambiguous_numerals, 2);
        assert_eq!(
            job.status,
            RepinJobStatus::Running,
            "staging is not a terminal transition"
        );

        let (services, rows) = staged(&pool, id).await;
        assert_eq!(
            services.unwrap(),
            vec!["nginx".to_owned(), "api".to_owned()]
        );
        assert_eq!(rows.unwrap(), vec![25, 6], "order is the caller's, kept");

        s.stage_cutover_input(id, JobTotals::default(), &[])
            .await
            .unwrap();
        assert_eq!(
            staged(&pool, id).await,
            (Some(Vec::new()), Some(Vec::new()))
        );
    }

    #[sqlx::test]
    async fn stage_cutover_input_is_idempotent(pool: PgPool) {
        let s = store(&pool);
        let id = running_execution(&pool, "duration").await;
        let totals = JobTotals {
            files_done: 3,
            rows_rewritten: 90,
            rows_nulled: 5,
            rows_resurrected: 0,
            ambiguous_numerals: 0,
        };
        let tallies = [("nginx".to_owned(), 5)];

        s.stage_cutover_input(id, totals, &tallies).await.unwrap();
        let first = staged(&pool, id).await;
        s.stage_cutover_input(id, totals, &tallies)
            .await
            .expect("re-staging a running job is a plain overwrite");
        assert_eq!(staged(&pool, id).await, first);
        assert_eq!(s.get(id).await.unwrap().unwrap().rows_nulled, 5);
    }

    #[sqlx::test]
    async fn staged_tallies_must_agree_on_cardinality(pool: PgPool) {
        let id = running_execution(&pool, "duration").await;

        let err = sqlx::query(
            "UPDATE repin_jobs
             SET nulled_services = ARRAY['nginx', 'api'], nulled_service_rows = ARRAY[25::bigint]
             WHERE id = $1",
        )
        .bind(id)
        .execute(&pool)
        .await
        .expect_err("mismatched cardinality must be refused");
        assert_check(&err, "repin_jobs_nulled_tallies_cardinality_check");
    }

    #[sqlx::test]
    async fn staged_tallies_are_present_or_absent_together(pool: PgPool) {
        let id = running_execution(&pool, "duration").await;

        let err =
            sqlx::query("UPDATE repin_jobs SET nulled_services = ARRAY['nginx'] WHERE id = $1")
                .bind(id)
                .execute(&pool)
                .await
                .expect_err("one column without the other must be refused");
        assert_check(&err, "repin_jobs_nulled_tallies_paired_check");
    }

    fn assert_check(err: &sqlx::Error, constraint: &str) {
        let sqlx::Error::Database(db) = err else {
            panic!("expected a database error, got {err:?}");
        };
        assert_eq!(db.code().as_deref(), Some("23514"), "{db:?}");
        assert_eq!(db.constraint(), Some(constraint), "{db:?}");
    }

    #[sqlx::test]
    async fn stage_cutover_input_refuses_a_dry_run(pool: PgPool) {
        let s = store(&pool);
        pin(&pool, "duration", CanonicalType::Varchar).await;
        let id = s
            .claim(RepinClaim {
                field: "duration",
                from_type: CanonicalType::Varchar,
                to_type: CanonicalType::BigInt,
                dialect: None,
                dry_run: true,
                force: false,
                max_nulled_rows: None,
                max_ambiguous_rows: None,
                requester: None,
            })
            .await
            .unwrap();

        let err = s
            .stage_cutover_input(id, JobTotals::default(), &[])
            .await
            .expect_err("a dry run has no cutover input to stage");
        assert!(matches!(err, StoreError::Validation(_)), "{err:?}");
        assert_eq!(staged(&pool, id).await, (None, None));
    }

    #[sqlx::test]
    async fn stage_cutover_input_refuses_an_already_terminal_job(pool: PgPool) {
        let s = store(&pool);
        let id = running_execution(&pool, "duration").await;
        s.finish_if_running(id, RepinJobStatus::Failed, Some("orphaned"), None)
            .await
            .unwrap();

        let err = s
            .stage_cutover_input(id, JobTotals::default(), &[])
            .await
            .expect_err("a terminal job must not accept staged tallies");
        assert!(matches!(err, StoreError::Validation(_)), "{err:?}");
        assert_eq!(staged(&pool, id).await, (None, None));
    }
}

/// Read both halves of a saved query back and assert the pair ADR-0018
/// ruling 7 forbids is not on disk.
async fn assert_pair_is_legal(
    saved_store: &SavedQueryStore,
    sched_store: &ScheduleStore,
    sq_id: i64,
) {
    let dsl = saved_store.get(sq_id, 1).await.unwrap().unwrap().query;
    let window = sched_store
        .get_schedule_for_saved_query(sq_id, 1)
        .await
        .unwrap()
        .and_then(|s| s.window);
    assert_eq!(
        trawl_server::report_window::validate_window_compatibility(window, &dsl),
        Ok(()),
        "committed pair is forbidden: window {window:?} over {dsl:?}"
    );
}

/// ADR-0018 ruling 7 forbids exactly one pair — a schedule window over a
/// query that spells its own interval — and the two write doors that could
/// assemble it run concurrently.
///
/// Neither door can see the other's uncommitted row, so what makes the rule
/// hold is that both take the `saved_queries` lock FIRST: the loser waits,
/// then reads what the winner committed and refuses. The blocker
/// transaction pins the interleaving to the dangerous window by holding
/// that row exactly as the other door would while its write is in flight.
///
/// Both orders are exercised, and the invariant is asserted with the rule
/// itself: whatever committed, `validate_window_compatibility` must accept
/// the pair that is on disk at the end.
#[sqlx::test]
async fn schedule_put_and_saved_put_cannot_commit_the_forbidden_pair(pool: PgPool) {
    let saved_store = saved(&pool);
    let sched_store = schedules(&pool);

    let first = saved_store
        .create(1, "pair-dsl-first", "service=x")
        .await
        .unwrap();
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query_scalar::<_, i64>("SELECT id FROM saved_queries WHERE id = $1 FOR UPDATE")
        .bind(first.id)
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    sqlx::query("UPDATE saved_queries SET query = $1 WHERE id = $2")
        .bind("service=x last=1h")
        .bind(first.id)
        .execute(&mut *blocker)
        .await
        .unwrap();

    let window_writer = sched_store.clone();
    let first_id = first.id;
    let now = chrono::Utc::now();
    let task = tokio::spawn(async move {
        window_writer
            .set_schedule_checked(
                first_id,
                1,
                3600,
                None,
                true,
                Some(ScheduleWindow::SinceLast),
                0,
                now,
            )
            .await
    });

    // Let the schedule write reach the lock before the edit commits.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    blocker.commit().await.unwrap();

    let err = task
        .await
        .unwrap()
        .expect_err("the window must be refused against the DSL that committed first");
    assert!(matches!(err, WindowWriteError::Policy(_)), "got {err:?}");
    assert!(err.to_string().contains("last="), "{err}");
    assert!(
        sched_store
            .get_schedule_for_saved_query(first.id, 1)
            .await
            .unwrap()
            .is_none(),
        "a refused set_schedule writes no schedule at all"
    );
    assert_pair_is_legal(&saved_store, &sched_store, first.id).await;

    // The blocker inserts the schedule by hand on the connection holding the
    // parent row: the FK would otherwise queue behind its own lock.
    let second = saved_store
        .create(1, "pair-window-first", "service=x")
        .await
        .unwrap();
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query_scalar::<_, i64>("SELECT id FROM saved_queries WHERE id = $1 FOR UPDATE")
        .bind(second.id)
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO schedules
             (saved_query_id, key_id, interval_secs, max_runs, enabled,
              window_kind, window_secs, lag_secs, covered_through, next_fire_at,
              created_at, updated_at)
         VALUES ($1, 1, 3600, NULL, TRUE, 'since_last', NULL, 0, NULL, now(), now(), now())",
    )
    .bind(second.id)
    .execute(&mut *blocker)
    .await
    .unwrap();

    let dsl_writer = saved_store.clone();
    let second_id = second.id;
    let task = tokio::spawn(async move {
        dsl_writer
            .update_checked(second_id, 1, "service=x last=1h", None)
            .await
    });

    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    blocker.commit().await.unwrap();

    let err = task
        .await
        .unwrap()
        .expect_err("the edit must be refused against the window that committed first");
    assert!(matches!(err, WindowWriteError::Policy(_)), "got {err:?}");
    assert!(err.to_string().contains("since_last"), "{err}");
    assert_eq!(
        saved_store.get(second.id, 1).await.unwrap().unwrap().query,
        "service=x",
        "a refused update stores nothing"
    );
    assert_pair_is_legal(&saved_store, &sched_store, second.id).await;
}

/// Every path that writes a `report_runs` row has to take the
/// `saved_queries` lock first: the insert's foreign key check takes FOR KEY
/// SHARE on the parent row, and the other order deadlocks (40P01) against
/// `set_schedule_checked`.
#[sqlx::test]
async fn manual_claim_and_schedule_write_do_not_deadlock(pool: PgPool) {
    let saved_store = saved(&pool);
    let sched_store = schedules(&pool);
    let sq = saved_store
        .create(1, "manual-order", "service=x")
        .await
        .unwrap();
    sched_store
        .create_schedule(sq.id, 1, 3600, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();

    // Level 1, held open: the first thing `set_schedule_checked` does.
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query_scalar::<_, i64>("SELECT id FROM saved_queries WHERE id = $1 FOR UPDATE")
        .bind(sq.id)
        .fetch_one(&mut *blocker)
        .await
        .unwrap();

    let claimant = sched_store.clone();
    let sq_id = sq.id;
    let claim = tokio::spawn(async move {
        claimant
            .claim_manual_run(sq_id, 1, 24, &RunLease::unattached())
            .await
    });

    // Let the claim reach whatever it blocks on.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    // Level 2, from the transaction that already holds level 1.
    let second_level = sqlx::query_scalar::<_, i64>(
        "SELECT id FROM schedules WHERE saved_query_id = $1 FOR UPDATE",
    )
    .bind(sq.id)
    .fetch_one(&mut *blocker)
    .await;
    assert!(
        second_level.is_ok(),
        "the schedule write deadlocked against the manual claim: {:?}",
        second_level.err()
    );
    blocker.commit().await.unwrap();

    let claimed = claim
        .await
        .unwrap()
        .expect("the manual claim must not deadlock either");
    assert!(
        matches!(claimed, ManualRunClaim::Started(_)),
        "got {claimed:?}"
    );
}

#[sqlx::test]
async fn history_clear_is_scoped_counted_and_preserves_saved(pool: PgPool) {
    let store = history(&pool);
    for query in ["first", "second"] {
        store
            .record_query(1, query, 12, 3, RunStatus::Success)
            .await
            .unwrap();
    }
    store
        .record_query(2, "foreign", 45, 6, RunStatus::Error)
        .await
        .unwrap();
    let foreign = store.get_user_history(2, 10, 0, None).await.unwrap();
    let saved_store = saved(&pool);
    let saved_query = saved_store.create(1, "keep", "*").await.unwrap();

    let deleted = store.clear_user_history(1).await.unwrap();
    assert_eq!(
        store.get_user_history(2, 10, 0, None).await.unwrap(),
        foreign,
        "history_clear preserves other keys"
    );
    assert_eq!(deleted, 2);
    assert_eq!(
        store.get_user_history(1, 10, 0, None).await.unwrap().total,
        0
    );
    assert_eq!(
        store.get_user_history(2, 10, 0, None).await.unwrap(),
        foreign
    );
    assert_eq!(saved_store.list(1).await.unwrap(), vec![saved_query]);
    assert_eq!(store.clear_user_history(1).await.unwrap(), 0);
    assert_eq!(
        store.get_user_history(2, 10, 0, None).await.unwrap(),
        foreign
    );
}

/// Deliberately interleave primary values across pages. Expected IDs are fixed
/// independently of the SQL comparator, including null and timestamp ties.
#[sqlx::test]
async fn list_all_runs_every_order_before_pagination(pool: PgPool) {
    use trawl_api::{
        RunsSortDir::{Asc, Desc},
        RunsSortKey::{Duration, Net, Rows, Started, Status},
    };
    let store = schedules(&pool);
    let mut nets = Vec::new();
    for (owner, name) in [
        (1, "beta"),
        (1, "alpha"),
        (1, "Zulu"),
        (2, "AAA-other-owner"),
    ] {
        let saved_id = seed_saved(&pool, owner, name).await;
        let schedule = store
            .create_schedule(saved_id, owner, 300, None, None, 0, chrono::Utc::now())
            .await
            .unwrap();
        nets.push((saved_id, schedule.id));
    }
    let fixtures = [
        (0, "success", 1, Some(10_i64), Some(2_i64)),
        (1, "error", 3, Some(2), Some(10)),
        (1, "success", 2, Some(10), Some(2)),
        (2, "timeout", 3, Some(100), Some(100)),
        (0, "running", 4, None, None),
        (1, "error", 3, Some(2), Some(10)),
        (1, "success", 5, Some(0), Some(0)),
        (2, "error", 0, None, None),
        (3, "success", 9, Some(999), Some(999)),
    ];
    let mut ids = Vec::new();
    for (net, status, seconds, duration, rows) in fixtures {
        let (saved_id, schedule_id) = nets[net];
        let id: i64 = sqlx::query_scalar("INSERT INTO report_runs
            (schedule_id, saved_query_id, query, status, started_at, duration_ms, row_count)
            VALUES ($1, $2, '*', $3, '2026-09-15T00:00:00Z'::timestamptz + $4 * interval '1 second', $5, $6) RETURNING id")
            .bind(schedule_id).bind(saved_id).bind(status).bind(f64::from(seconds)).bind(duration).bind(rows)
            .fetch_one(&pool).await.unwrap();
        ids.push(id);
    }
    for (key, dir, expected) in [
        (Net, Asc, [6, 5, 1, 2, 4, 0, 3, 7]),
        (Net, Desc, [3, 7, 4, 0, 6, 5, 1, 2]),
        (Status, Asc, [5, 1, 7, 4, 6, 2, 0, 3]),
        (Status, Desc, [3, 6, 2, 0, 4, 5, 1, 7]),
        (Started, Asc, [7, 0, 2, 1, 3, 5, 4, 6]),
        (Started, Desc, [6, 4, 5, 3, 1, 2, 0, 7]),
        (Duration, Asc, [6, 5, 1, 2, 0, 3, 4, 7]),
        (Duration, Desc, [3, 2, 0, 5, 1, 6, 4, 7]),
        (Rows, Asc, [6, 2, 0, 5, 1, 3, 4, 7]),
        (Rows, Desc, [3, 5, 1, 2, 0, 6, 4, 7]),
    ] {
        let mut actual = Vec::new();
        for offset in [0, 3, 6, 9] {
            let page = store.list_all_runs(1, 3, offset, key, dir).await.unwrap();
            assert!(page.len() <= 3);
            actual.extend(page.into_iter().map(|(run, _)| run.id));
        }
        assert_eq!(actual, expected.map(|i| ids[i]), "{key:?} {dir:?}");
        let other = store.list_all_runs(2, 3, 0, key, dir).await.unwrap();
        assert_eq!(
            other.iter().map(|(run, _)| run.id).collect::<Vec<_>>(),
            vec![ids[8]]
        );
    }
    assert_eq!(store.count_all_runs(1).await.unwrap(), 8);
}
