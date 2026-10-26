// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres integration tests for fleet-auth's `KeyStore` under the
//! roles-as-data model (ADR-0006).

#![cfg(feature = "keystore")]

#[path = "support/answer_hold.rs"]
mod answer_hold;

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fleet_auth::{
    AuthError, KeyStore, PrincipalKind, RolePermission, token, validate_app_namespace,
};

#[derive(Clone, Default)]
struct EventCapture {
    event_types: Arc<Mutex<Vec<String>>>,
}

#[derive(Default)]
struct EventTypeVisitor {
    event_type: Option<String>,
}

impl tracing::field::Visit for EventTypeVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "event_type" {
            self.event_type = Some(value.to_owned());
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "event_type" {
            self.event_type = Some(format!("{value:?}"));
        }
    }
}

impl<S> tracing_subscriber::Layer<S> for EventCapture
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = EventTypeVisitor::default();
        event.record(&mut visitor);
        if let Some(event_type) = visitor.event_type {
            self.event_types
                .lock()
                .expect("event capture mutex")
                .push(event_type);
        }
    }
}

async fn capture_event_types<T>(future: impl Future<Output = T>) -> (T, Vec<String>) {
    use tracing::instrument::WithSubscriber as _;
    use tracing_subscriber::layer::SubscriberExt as _;

    let capture = EventCapture::default();
    let event_types = Arc::clone(&capture.event_types);
    let subscriber = tracing_subscriber::registry().with(capture);
    let result = future.with_subscriber(subscriber).await;
    let captured = event_types.lock().expect("event capture mutex").clone();
    (result, captured)
}

async fn key_role_names(pool: &sqlx::PgPool, key_id: i64) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT r.name
         FROM key_roles kr
         JOIN roles r ON r.id = kr.role_id
         WHERE kr.key_id = $1
         ORDER BY r.name",
    )
    .bind(key_id)
    .fetch_all(pool)
    .await
    .expect("read key roles")
}

async fn wait_for_key_lock_waiters(pool: &sqlx::PgPool, expected: i64) {
    wait_for_key_lock_waiters_matching(pool, expected, "%api_keys%").await;
}

async fn wait_for_role_assignment_lock_waiters(pool: &sqlx::PgPool, expected: i64) {
    wait_for_key_lock_waiters_matching(
        pool,
        expected,
        "%SELECT id, active%FROM api_keys%FOR UPDATE%",
    )
    .await;
}

async fn wait_for_key_lock_waiters_matching(
    pool: &sqlx::PgPool,
    expected: i64,
    query_pattern: &str,
) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT COUNT(*)
                 FROM pg_stat_activity
                 WHERE datname = current_database()
                   AND pid <> pg_backend_pid()
                   AND wait_event_type = 'Lock'
                   AND query LIKE $1",
            )
            .bind(query_pattern)
            .fetch_one(pool)
            .await
            .expect("inspect Postgres lock waiters");
            if waiting >= expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("operations must reach the held api_keys row lock");
}

fn rp(app: &str, permission: &str) -> RolePermission {
    RolePermission {
        app: app.into(),
        permission: permission.into(),
    }
}

fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| (*s).to_owned()).collect()
}

/// Seed the standard converted-shape roles used across these tests.
async fn seed_roles(store: &KeyStore) {
    store
        .create_role(
            "trawl-admin",
            None,
            &[
                rp("trawl", "query"),
                rp("trawl", "schema_read"),
                rp("trawl", "server_manage"),
            ],
        )
        .await
        .expect("seed trawl-admin");
    store
        .create_role(
            "trawl-analyst",
            None,
            &[rp("trawl", "query"), rp("trawl", "schema_read")],
        )
        .await
        .expect("seed trawl-analyst");
    store
        .create_role(
            "coastwatch-siem_consumer",
            None,
            &[rp("coastwatch", "ioc_exports_read")],
        )
        .await
        .expect("seed coastwatch-siem_consumer");
}

// ---------------------------------------------------------------------------
// lifecycle tests
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn create_and_verify_roundtrip(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let created = store
        .create_key("test", PrincipalKind::Human, &names(&["trawl-admin"]), None)
        .await
        .expect("create");
    assert!(created.plaintext_token.starts_with("flt_"));
    assert_eq!(created.info.roles, ["trawl-admin"]);

    let verified = store
        .verify_key(&created.plaintext_token)
        .await
        .expect("verify");
    assert_eq!(verified.name, "test");
    assert_eq!(verified.kind, PrincipalKind::Human);
    assert_eq!(verified.roles(), ["trawl-admin"]);
    assert!(verified.has_app_permission("trawl", "server_manage"));
    assert!(!verified.has_app_permission("trawl", "ingest"));
}

#[sqlx::test]
async fn multi_role_key_resolves_the_union(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    store
        .create_role(
            "tier1",
            None,
            &[rp("trawl", "query"), rp("trawl", "schema_read")],
        )
        .await
        .unwrap();
    store
        .create_role(
            "tier2",
            None,
            &[rp("trawl", "query"), rp("trawl", "export")],
        )
        .await
        .unwrap();

    let created = store
        .create_key(
            "tiered",
            PrincipalKind::Human,
            &names(&["tier1", "tier2"]),
            None,
        )
        .await
        .unwrap();
    let verified = store.verify_key(&created.plaintext_token).await.unwrap();
    assert_eq!(verified.roles(), ["tier1", "tier2"]);
    assert_eq!(
        verified.permissions_for("trawl"),
        ["export", "query", "schema_read"]
    );
}

#[sqlx::test]
async fn cross_app_role_grants_in_both_namespaces(pool: sqlx::PgPool) {
    // One role spanning two apps grants in both from a single key_roles row.
    let store = KeyStore::from_pool(pool);
    store
        .create_role(
            "bridge",
            None,
            &[rp("trawl", "query"), rp("coastwatch", "stories_read")],
        )
        .await
        .unwrap();

    let created = store
        .create_key(
            "spanning",
            PrincipalKind::Service,
            &names(&["bridge"]),
            None,
        )
        .await
        .unwrap();
    let verified = store.verify_key(&created.plaintext_token).await.unwrap();
    assert!(verified.has_app_permission("trawl", "query"));
    assert!(verified.has_app_permission("coastwatch", "stories_read"));
    assert!(verified.has_any_permission("trawl"));
    assert!(verified.has_any_permission("coastwatch"));
}

#[sqlx::test]
async fn create_key_with_unknown_role_is_role_not_found(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let err = store
        .create_key("typo", PrincipalKind::Human, &names(&["trawl-adnim"]), None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, AuthError::RoleNotFound { ref name } if name == "trawl-adnim"),
        "got {err:?}"
    );
}

#[sqlx::test]
async fn duplicate_role_in_batch_rejected(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let err = store
        .create_key(
            "dup",
            PrincipalKind::Human,
            &names(&["trawl-admin", "trawl-admin"]),
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::RoleAlreadyAssigned { .. }));
}

#[sqlx::test]
async fn empty_roles_create_then_verify(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let created = store
        .create_key("empty", PrincipalKind::Service, &[], None)
        .await
        .expect("create");
    let verified = store.verify_key(&created.plaintext_token).await.unwrap();
    assert!(verified.roles().is_empty());
    assert!(!verified.has_any_permission("trawl"));
    assert_eq!(verified.roles_display(), "none");
}

#[sqlx::test]
async fn create_key_with_expiry_in_future(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let created = store
        .create_key(
            "expiring",
            PrincipalKind::Human,
            &names(&["trawl-admin"]),
            Some(Duration::from_hours(24)),
        )
        .await
        .expect("create");
    assert!(created.info.expires_at.is_some());
    let verified = store.verify_key(&created.plaintext_token).await.unwrap();
    assert_eq!(verified.name, "expiring");
}

// ---------------------------------------------------------------------------
// role CRUD
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn create_role_roundtrip_with_rate_rpm(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let role = store
        .create_role("shipper", Some(2000), &[rp("trawl", "ingest")])
        .await
        .expect("create role");
    assert_eq!(role.name, "shipper");
    assert_eq!(role.rate_rpm, Some(2000));
    assert_eq!(role.permissions, vec![rp("trawl", "ingest")]);

    let fetched = store.get_role("shipper").await.unwrap();
    assert_eq!(fetched, role);
}

#[sqlx::test]
async fn create_role_duplicate_name_is_role_exists(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    store.create_role("dup", None, &[]).await.unwrap();
    let err = store.create_role("dup", None, &[]).await.unwrap_err();
    assert!(matches!(err, AuthError::RoleExists { ref name } if name == "dup"));
}

#[sqlx::test]
async fn create_role_rejects_bad_shapes(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let err = store.create_role("Bad Name", None, &[]).await.unwrap_err();
    assert!(matches!(err, AuthError::InvalidRole(_)));

    let err = store
        .create_role("ok", None, &[rp("Trawl", "query")])
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::InvalidApp(_)));

    let err = store
        .create_role("ok", None, &[rp("trawl", "Query!")])
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::InvalidPermission(_)));
}

#[sqlx::test]
async fn list_roles_ordered_with_bundles(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let roles = store.list_roles().await.unwrap();
    let names: Vec<&str> = roles.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(
        names,
        ["coastwatch-siem_consumer", "trawl-admin", "trawl-analyst"]
    );
    let admin = roles.iter().find(|r| r.name == "trawl-admin").unwrap();
    assert_eq!(admin.permissions.len(), 3);
}

#[sqlx::test]
async fn get_role_unknown_is_role_not_found(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    let err = store.get_role("ghost").await.unwrap_err();
    assert!(matches!(err, AuthError::RoleNotFound { ref name } if name == "ghost"));
}

#[sqlx::test]
async fn add_and_remove_role_permissions_reflected_on_next_verify(pool: sqlx::PgPool) {
    // Permissions resolve fresh on every verify_key, never cached, so a role
    // mutation is visible to the very next call.
    let store = KeyStore::from_pool(pool);
    store
        .create_role("mutable", None, &[rp("trawl", "query")])
        .await
        .unwrap();
    let created = store
        .create_key(
            "watcher",
            PrincipalKind::Service,
            &names(&["mutable"]),
            None,
        )
        .await
        .unwrap();

    let before = store.verify_key(&created.plaintext_token).await.unwrap();
    assert!(!before.has_app_permission("trawl", "export"));

    store
        .add_role_permissions("mutable", &[rp("trawl", "export")])
        .await
        .unwrap();
    let after = store.verify_key(&created.plaintext_token).await.unwrap();
    assert!(after.has_app_permission("trawl", "export"));

    let removed = store
        .remove_role_permissions("mutable", &[rp("trawl", "export")])
        .await
        .unwrap();
    assert_eq!(removed, 1);
    let final_state = store.verify_key(&created.plaintext_token).await.unwrap();
    assert!(!final_state.has_app_permission("trawl", "export"));

    // Removing a pair that isn't on the role reports zero rows.
    let removed = store
        .remove_role_permissions("mutable", &[rp("trawl", "export")])
        .await
        .unwrap();
    assert_eq!(removed, 0);
}

#[sqlx::test]
async fn set_role_rate_rpm_updates_in_place_without_touching_assignments(pool: sqlx::PgPool) {
    // Re-tiering class-of-service must not cost an authz outage: the key keeps
    // the role, and its permissions, across the change.
    let store = KeyStore::from_pool(pool);
    store
        .create_role("tier", Some(120), &[rp("trawl", "query")])
        .await
        .unwrap();
    let created = store
        .create_key("shipper", PrincipalKind::Service, &names(&["tier"]), None)
        .await
        .unwrap();
    let before = store.verify_key(&created.plaintext_token).await.unwrap();
    assert_eq!(before.rate_rpm(), Some(120));

    let updated = store.set_role_rate_rpm("tier", Some(2000)).await.unwrap();
    assert_eq!(updated.rate_rpm, Some(2000));
    assert_eq!(updated.permissions, vec![rp("trawl", "query")]);

    let verified = store.verify_key(&created.plaintext_token).await.unwrap();
    assert_eq!(verified.rate_rpm(), Some(2000));
    assert_eq!(verified.roles(), ["tier"]);
    assert!(verified.has_app_permission("trawl", "query"));

    // `None` clears the override back to the route-class config defaults.
    let cleared = store.set_role_rate_rpm("tier", None).await.unwrap();
    assert_eq!(cleared.rate_rpm, None);
    let after = store.verify_key(&created.plaintext_token).await.unwrap();
    assert_eq!(after.rate_rpm(), None);
    assert_eq!(store.count_role_assignments("tier").await.unwrap(), 1);
}

#[sqlx::test]
async fn set_role_rate_rpm_unknown_is_role_not_found(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    let err = store
        .set_role_rate_rpm("ghost", Some(60))
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::RoleNotFound { ref name } if name == "ghost"));
}

#[sqlx::test]
async fn delete_role_refuses_while_assigned_unless_forced(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    store
        .create_role("clingy", None, &[rp("trawl", "query")])
        .await
        .unwrap();
    let created = store
        .create_key("holder", PrincipalKind::Service, &names(&["clingy"]), None)
        .await
        .unwrap();

    let err = store.delete_role("clingy", false).await.unwrap_err();
    assert!(
        matches!(err, AuthError::RoleInUse { ref name, key_count } if name == "clingy" && key_count == 1),
        "got {err:?}"
    );
    assert_eq!(store.count_role_assignments("clingy").await.unwrap(), 1);

    // Forced delete unassigns and removes; the key loses the capability.
    store.delete_role("clingy", true).await.unwrap();
    let err = store.get_role("clingy").await.unwrap_err();
    assert!(matches!(err, AuthError::RoleNotFound { .. }));
    let verified = store.verify_key(&created.plaintext_token).await.unwrap();
    assert!(verified.roles().is_empty());
}

#[sqlx::test]
async fn delete_unassigned_role_succeeds_without_force(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    store.create_role("loose", None, &[]).await.unwrap();
    store.delete_role("loose", false).await.unwrap();
    assert!(matches!(
        store.get_role("loose").await.unwrap_err(),
        AuthError::RoleNotFound { .. }
    ));
}

#[sqlx::test]
async fn is_known_permission_reads_the_registry(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    // Seeded at migrate-time for the trawl namespace.
    assert!(store.is_known_permission("trawl", "query").await.unwrap());
    assert!(store.is_known_permission("trawl", "ingest").await.unwrap());
    assert!(
        !store
            .is_known_permission("trawl", "key_manage")
            .await
            .unwrap(),
        "dead key_manage is never seeded"
    );
    assert!(!store.is_known_permission("trawl", "qyery").await.unwrap());
    assert!(
        !store
            .is_known_permission("coastwatch", "stories_read")
            .await
            .unwrap(),
        "coastwatch seeds its own vocabulary in its companion arc"
    );
}

// ---------------------------------------------------------------------------
// verify_key security
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn verify_nonexistent_prefix_runs_dummy_and_fails(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let result = store
        .verify_key("flt_ZZZZZZZZthisisntarealkeyatallnopenope12")
        .await;
    assert!(matches!(result, Err(AuthError::InvalidKey(_))));
}

#[sqlx::test]
async fn malformed_token_rejected(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let result = store.verify_key("not-a-trawl-token").await;
    assert!(matches!(result, Err(AuthError::MalformedToken(_))));
}

#[sqlx::test]
async fn wrong_token_with_existing_prefix_fails(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let created = store
        .create_key(
            "legit",
            PrincipalKind::Human,
            &names(&["trawl-admin"]),
            None,
        )
        .await
        .unwrap();
    // Same prefix, different body must not verify. The cache key binds the
    // plaintext's fingerprint; keying it on (prefix, stored_hash) alone would
    // let this succeed once the legit token's verify populated the entry.
    store.verify_key(&created.plaintext_token).await.unwrap();
    let prefix_only = &created.plaintext_token[..12]; // "flt_" + 8
    let tampered = format!("{prefix_only}AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA0");
    let result = store.verify_key(&tampered).await;
    assert!(matches!(result, Err(AuthError::InvalidKey(_))));
}

#[sqlx::test]
async fn cache_hit_skips_kdf_but_updates_last_used(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let created = store
        .create_key(
            "cacheable",
            PrincipalKind::Human,
            &names(&["trawl-admin"]),
            None,
        )
        .await
        .unwrap();

    let before = store.cache_stats().entries;
    store.verify_key(&created.plaintext_token).await.unwrap();
    let after_first = store.cache_stats().entries;
    assert_eq!(after_first, before + 1, "first verify populates cache");

    let info = store.get_key_by_prefix(&created.info.prefix).await.unwrap();
    let last_used_first = info.last_used.expect("set by verify");

    // Brief sleep so the next timestamp can differ (NOW() is microsecond-precise).
    tokio::time::sleep(Duration::from_millis(20)).await;

    store.verify_key(&created.plaintext_token).await.unwrap();
    let after_second = store.cache_stats().entries;
    assert_eq!(after_second, after_first, "cache hit, no new entry");

    let info_second = store.get_key_by_prefix(&created.info.prefix).await.unwrap();
    let last_used_second = info_second.last_used.expect("still set");
    assert!(
        last_used_second > last_used_first,
        "cache hit still updates last_used"
    );
}

#[sqlx::test]
async fn revoke_blocks_verify_even_with_cache_entry(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let created = store
        .create_key(
            "revokable",
            PrincipalKind::Human,
            &names(&["trawl-admin"]),
            None,
        )
        .await
        .unwrap();
    // Populate cache.
    store.verify_key(&created.plaintext_token).await.unwrap();

    store.revoke_key(&created.info.prefix).await.unwrap();

    // Cache still has the entry, but the conditional UPDATE rejects.
    let result = store.verify_key(&created.plaintext_token).await;
    assert!(matches!(result, Err(AuthError::InvalidKey(_))));
}

#[sqlx::test]
async fn expired_key_fails_verify(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    // Can't backdate expires_at directly — the api_keys_expiry_after_create
    // CHECK constraint refuses any expires_at <= created_at. Instead, create
    // with a near-zero TTL and sleep past it before verifying.
    let created = store
        .create_key(
            "doomed",
            PrincipalKind::Human,
            &names(&["trawl-admin"]),
            Some(Duration::from_millis(50)),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let result = store.verify_key(&created.plaintext_token).await;
    assert!(matches!(result, Err(AuthError::InvalidKey(_))));
}

#[sqlx::test]
async fn toctou_revoke_between_select_and_update_rejects(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let created = store
        .create_key("racy", PrincipalKind::Human, &names(&["trawl-admin"]), None)
        .await
        .unwrap();
    // Populate cache via one successful verify, then revoke directly through
    // SQL so we don't bust the cache, then verify again. The conditional
    // last_used UPDATE catches the revocation.
    store.verify_key(&created.plaintext_token).await.unwrap();
    sqlx::query("UPDATE api_keys SET active = FALSE WHERE id = $1")
        .bind(created.info.id)
        .execute(store.pool())
        .await
        .unwrap();
    let result = store.verify_key(&created.plaintext_token).await;
    assert!(matches!(result, Err(AuthError::InvalidKey(_))));
}

#[sqlx::test]
async fn concurrent_verifies_all_succeed(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let created = store
        .create_key(
            "concurrent",
            PrincipalKind::Human,
            &names(&["trawl-admin"]),
            None,
        )
        .await
        .unwrap();
    let token = created.plaintext_token.to_string();

    let mut handles = Vec::new();
    for _ in 0..16 {
        let s = store.clone();
        let t = token.clone();
        handles.push(tokio::spawn(async move { s.verify_key(&t).await }));
    }
    for h in handles {
        h.await.unwrap().expect("each verify succeeds");
    }
}

/// An in-flight `verify_key` holds the `api_keys` row lock, so a concurrent
/// role unassignment on the same key has to wait for it.
#[sqlx::test]
async fn unassign_role_waits_for_key_row_lock(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let created = store
        .create_key(
            "lock-step",
            PrincipalKind::Human,
            &names(&["trawl-admin", "coastwatch-siem_consumer"]),
            None,
        )
        .await
        .unwrap();

    // `verify_key` holds this same row lock while it resolves roles.
    // Holding it manually gives the regression test a deterministic
    // interleaving instead of hoping the scheduler lands inside a
    // microsecond race window.
    let mut tx = store.pool().begin().await.unwrap();
    sqlx::query("SELECT id FROM api_keys WHERE id = $1 FOR UPDATE")
        .bind(created.info.id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();

    let unassign_store = store.clone();
    let prefix = created.info.prefix.clone();
    let unassign = tokio::spawn(async move {
        unassign_store
            .unassign_role(&prefix, "coastwatch-siem_consumer")
            .await
    });

    wait_for_key_lock_waiters(store.pool(), 1).await;

    tx.commit().await.unwrap();
    unassign.await.unwrap().unwrap();

    let verified = store.verify_key(&created.plaintext_token).await.unwrap();
    assert_eq!(verified.roles(), ["trawl-admin"]);
    assert!(!verified.has_any_permission("coastwatch"));
}

#[sqlx::test]
async fn concurrent_distinct_role_assignments_both_persist(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let created = store
        .create_key("distinct-roles", PrincipalKind::Human, &[], None)
        .await
        .unwrap();
    let mut blocker = store.pool().begin().await.unwrap();
    sqlx::query("SELECT id FROM api_keys WHERE id = $1 FOR UPDATE")
        .bind(created.info.id)
        .fetch_one(&mut *blocker)
        .await
        .unwrap();

    let first_store = store.clone();
    let first_prefix = created.info.prefix.clone();
    let first =
        tokio::spawn(async move { first_store.assign_role(&first_prefix, "trawl-admin").await });
    let second_store = store.clone();
    let second_prefix = created.info.prefix.clone();
    let second = tokio::spawn(async move {
        second_store
            .assign_role(&second_prefix, "coastwatch-siem_consumer")
            .await
    });

    wait_for_role_assignment_lock_waiters(store.pool(), 2).await;
    blocker.commit().await.unwrap();

    first.await.unwrap().expect("first assignment must succeed");
    second
        .await
        .unwrap()
        .expect("second assignment must succeed");
    assert_eq!(
        key_role_names(store.pool(), created.info.id).await,
        ["coastwatch-siem_consumer", "trawl-admin"]
    );
}

#[sqlx::test]
async fn concurrent_duplicate_role_assignment_has_one_winner(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let created = store
        .create_key("duplicate-role", PrincipalKind::Human, &[], None)
        .await
        .unwrap();
    let mut blocker = store.pool().begin().await.unwrap();
    sqlx::query("SELECT id FROM api_keys WHERE id = $1 FOR UPDATE")
        .bind(created.info.id)
        .fetch_one(&mut *blocker)
        .await
        .unwrap();

    let first_store = store.clone();
    let first_prefix = created.info.prefix.clone();
    let first =
        tokio::spawn(async move { first_store.assign_role(&first_prefix, "trawl-admin").await });
    let second_store = store.clone();
    let second_prefix = created.info.prefix.clone();
    let second = tokio::spawn(async move {
        second_store
            .assign_role(&second_prefix, "trawl-admin")
            .await
    });

    wait_for_role_assignment_lock_waiters(store.pool(), 2).await;
    blocker.commit().await.unwrap();

    let mut succeeded = 0;
    let mut duplicate_errors = 0;
    for result in [first.await.unwrap(), second.await.unwrap()] {
        match result {
            Ok(()) => succeeded += 1,
            Err(AuthError::RoleAlreadyAssigned { prefix, role }) => {
                assert_eq!(prefix, created.info.prefix);
                assert_eq!(role, "trawl-admin");
                duplicate_errors += 1;
            }
            Err(error) => panic!("unexpected assignment error: {error:?}"),
        }
    }
    assert_eq!(succeeded, 1);
    assert_eq!(duplicate_errors, 1);
    assert_eq!(
        key_role_names(store.pool(), created.info.id).await,
        ["trawl-admin"]
    );
}

// ---------------------------------------------------------------------------
// admin operations
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn revoke_nonexistent_prefix_errors(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let result = store.revoke_key("ZZZZZZZZ").await;
    assert!(matches!(result, Err(AuthError::KeyNotFound { .. })));
}

#[sqlx::test]
async fn double_revoke_errors_with_kindly_message(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let created = store
        .create_key(
            "twice",
            PrincipalKind::Human,
            &names(&["trawl-admin"]),
            None,
        )
        .await
        .unwrap();
    store.revoke_key(&created.info.prefix).await.unwrap();
    let result = store.revoke_key(&created.info.prefix).await;
    assert!(matches!(result, Err(AuthError::KeyRevoked { .. })));
}

#[sqlx::test]
async fn list_keys_active_only_filters(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let keep = store
        .create_key("keep", PrincipalKind::Human, &names(&["trawl-admin"]), None)
        .await
        .unwrap();
    let drop_me = store
        .create_key("drop", PrincipalKind::Human, &names(&["trawl-admin"]), None)
        .await
        .unwrap();
    store.revoke_key(&drop_me.info.prefix).await.unwrap();

    let active = store.list_keys(true).await.unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].prefix, keep.info.prefix);
    assert_eq!(active[0].roles, ["trawl-admin"]);

    let all = store.list_keys(false).await.unwrap();
    assert_eq!(all.len(), 2);
}

#[sqlx::test]
async fn assign_and_unassign_role_roundtrip(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let created = store
        .create_key(
            "shared",
            PrincipalKind::Service,
            &names(&["trawl-admin"]),
            None,
        )
        .await
        .unwrap();
    store
        .assign_role(&created.info.prefix, "coastwatch-siem_consumer")
        .await
        .unwrap();

    let after = store.get_key_by_prefix(&created.info.prefix).await.unwrap();
    assert_eq!(after.roles, ["coastwatch-siem_consumer", "trawl-admin"]);

    store
        .unassign_role(&created.info.prefix, "coastwatch-siem_consumer")
        .await
        .unwrap();
    let after_unassign = store.get_key_by_prefix(&created.info.prefix).await.unwrap();
    assert_eq!(after_unassign.roles, ["trawl-admin"]);
}

#[sqlx::test]
async fn role_mutations_reject_revoked_key_without_state_or_success_event(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let created = store
        .create_key(
            "revoked",
            PrincipalKind::Human,
            &names(&["trawl-admin"]),
            None,
        )
        .await
        .unwrap();
    store.revoke_key(&created.info.prefix).await.unwrap();
    let before = key_role_names(store.pool(), created.info.id).await;

    let (assign, assign_events) =
        capture_event_types(store.assign_role(&created.info.prefix, "trawl-admin")).await;
    assert!(
        matches!(assign, Err(AuthError::KeyRevoked { ref prefix }) if prefix == &created.info.prefix)
    );
    assert!(!assign_events.iter().any(|event| event == "role_assigned"));
    assert_eq!(key_role_names(store.pool(), created.info.id).await, before);

    let (unassign, unassign_events) =
        capture_event_types(store.unassign_role(&created.info.prefix, "coastwatch-siem_consumer"))
            .await;
    assert!(
        matches!(unassign, Err(AuthError::KeyRevoked { ref prefix }) if prefix == &created.info.prefix)
    );
    assert!(
        !unassign_events
            .iter()
            .any(|event| event == "role_unassigned")
    );

    assert_eq!(key_role_names(store.pool(), created.info.id).await, before);
    assert!(matches!(
        store.verify_key(&created.plaintext_token).await,
        Err(AuthError::InvalidKey(_))
    ));
}

#[sqlx::test]
async fn role_mutations_reject_out_of_band_inactive_key_without_state_or_success_event(
    pool: sqlx::PgPool,
) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let created = store
        .create_key(
            "inactive",
            PrincipalKind::Human,
            &names(&["trawl-admin"]),
            None,
        )
        .await
        .unwrap();
    sqlx::query("UPDATE api_keys SET active = FALSE WHERE id = $1")
        .bind(created.info.id)
        .execute(store.pool())
        .await
        .unwrap();
    let before = key_role_names(store.pool(), created.info.id).await;

    let (assign, assign_events) =
        capture_event_types(store.assign_role(&created.info.prefix, "coastwatch-siem_consumer"))
            .await;
    assert!(
        matches!(assign, Err(AuthError::KeyRevoked { ref prefix }) if prefix == &created.info.prefix)
    );
    assert!(!assign_events.iter().any(|event| event == "role_assigned"));
    assert_eq!(key_role_names(store.pool(), created.info.id).await, before);

    let (unassign, unassign_events) =
        capture_event_types(store.unassign_role(&created.info.prefix, "trawl-admin")).await;
    assert!(
        matches!(unassign, Err(AuthError::KeyRevoked { ref prefix }) if prefix == &created.info.prefix)
    );
    assert!(
        !unassign_events
            .iter()
            .any(|event| event == "role_unassigned")
    );

    assert_eq!(key_role_names(store.pool(), created.info.id).await, before);
    let info = store.get_key_by_prefix(&created.info.prefix).await.unwrap();
    assert!(!info.active);
    assert!(info.revoked_at.is_none());
    assert!(matches!(
        store.verify_key(&created.plaintext_token).await,
        Err(AuthError::InvalidKey(_))
    ));
}

#[sqlx::test]
async fn expired_active_key_still_accepts_role_mutations(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let created = store
        .create_key(
            "expired",
            PrincipalKind::Human,
            &names(&["trawl-admin"]),
            None,
        )
        .await
        .unwrap();
    sqlx::query(
        "UPDATE api_keys
         SET created_at = NOW() - INTERVAL '2 seconds',
             expires_at = NOW() - INTERVAL '1 second'
         WHERE id = $1",
    )
    .bind(created.info.id)
    .execute(store.pool())
    .await
    .unwrap();

    assert!(matches!(
        store.verify_key(&created.plaintext_token).await,
        Err(AuthError::InvalidKey(_))
    ));
    let (assign, assign_events) =
        capture_event_types(store.assign_role(&created.info.prefix, "coastwatch-siem_consumer"))
            .await;
    assign.expect("expiry alone must not block assignment");
    assert!(assign_events.iter().any(|event| event == "role_assigned"));

    let (unassign, unassign_events) =
        capture_event_types(store.unassign_role(&created.info.prefix, "trawl-admin")).await;
    unassign.expect("expiry alone must not block unassignment");
    assert!(
        unassign_events
            .iter()
            .any(|event| event == "role_unassigned")
    );
    assert_eq!(
        key_role_names(store.pool(), created.info.id).await,
        ["coastwatch-siem_consumer"]
    );
}

#[sqlx::test]
async fn revoke_waiting_first_refuses_later_assignment(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let created = store
        .create_key("revoke-first", PrincipalKind::Human, &[], None)
        .await
        .unwrap();
    let mut blocker = store.pool().begin().await.unwrap();
    sqlx::query("SELECT id FROM api_keys WHERE id = $1 FOR UPDATE")
        .bind(created.info.id)
        .execute(&mut *blocker)
        .await
        .unwrap();

    let revoke_store = store.clone();
    let revoke_prefix = created.info.prefix.clone();
    let revoke = tokio::spawn(async move { revoke_store.revoke_key(&revoke_prefix).await });
    wait_for_key_lock_waiters(store.pool(), 1).await;

    let assign_store = store.clone();
    let assign_prefix = created.info.prefix.clone();
    let assign = tokio::spawn(async move {
        assign_store
            .assign_role(&assign_prefix, "trawl-admin")
            .await
    });
    wait_for_key_lock_waiters(store.pool(), 2).await;

    blocker.commit().await.unwrap();
    revoke.await.unwrap().expect("queued revoke must succeed");
    let err = assign.await.unwrap().unwrap_err();
    assert!(matches!(err, AuthError::KeyRevoked { ref prefix } if prefix == &created.info.prefix));
    assert!(
        key_role_names(store.pool(), created.info.id)
            .await
            .is_empty()
    );
}

#[sqlx::test]
async fn unassignment_waiting_first_commits_before_later_revoke(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let created = store
        .create_key(
            "mutation-first",
            PrincipalKind::Human,
            &names(&["trawl-admin", "coastwatch-siem_consumer"]),
            None,
        )
        .await
        .unwrap();
    let mut blocker = store.pool().begin().await.unwrap();
    sqlx::query("SELECT id FROM api_keys WHERE id = $1 FOR UPDATE")
        .bind(created.info.id)
        .execute(&mut *blocker)
        .await
        .unwrap();

    let unassign_store = store.clone();
    let unassign_prefix = created.info.prefix.clone();
    let unassign = tokio::spawn(async move {
        unassign_store
            .unassign_role(&unassign_prefix, "coastwatch-siem_consumer")
            .await
    });
    wait_for_key_lock_waiters(store.pool(), 1).await;

    let revoke_store = store.clone();
    let revoke_prefix = created.info.prefix.clone();
    let revoke = tokio::spawn(async move { revoke_store.revoke_key(&revoke_prefix).await });
    wait_for_key_lock_waiters(store.pool(), 2).await;

    blocker.commit().await.unwrap();
    unassign
        .await
        .unwrap()
        .expect("queued unassignment must commit first");
    revoke.await.unwrap().expect("later revoke must succeed");
    assert_eq!(
        key_role_names(store.pool(), created.info.id).await,
        ["trawl-admin"]
    );
    assert!(matches!(
        store.verify_key(&created.plaintext_token).await,
        Err(AuthError::InvalidKey(_))
    ));
}

#[sqlx::test]
async fn assign_rejects_duplicate_role(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let created = store
        .create_key("k", PrincipalKind::Human, &names(&["trawl-admin"]), None)
        .await
        .unwrap();
    let err = store
        .assign_role(&created.info.prefix, "trawl-admin")
        .await
        .unwrap_err();
    assert!(
        matches!(err, AuthError::RoleAlreadyAssigned { ref role, .. } if role == "trawl-admin")
    );
}

#[sqlx::test]
async fn assign_unknown_role_is_role_not_found(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let created = store
        .create_key("k", PrincipalKind::Human, &[], None)
        .await
        .unwrap();
    let err = store
        .assign_role(&created.info.prefix, "ghost-role")
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::RoleNotFound { ref name } if name == "ghost-role"));
}

#[sqlx::test]
async fn unassign_role_not_held_errors(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let created = store
        .create_key("k", PrincipalKind::Human, &names(&["trawl-admin"]), None)
        .await
        .unwrap();
    let err = store
        .unassign_role(&created.info.prefix, "trawl-analyst")
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::RoleNotAssigned { ref role, .. } if role == "trawl-analyst"));
}

#[sqlx::test]
async fn retype_key_succeeds(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let created = store
        .create_key(
            "morphing",
            PrincipalKind::Human,
            &names(&["trawl-admin"]),
            None,
        )
        .await
        .unwrap();
    let updated = store
        .retype_key(&created.info.prefix, PrincipalKind::Service)
        .await
        .unwrap();
    assert_eq!(updated.kind, PrincipalKind::Service);
}

#[sqlx::test]
async fn retype_revoked_key_errors(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let created = store
        .create_key("dead", PrincipalKind::Human, &names(&["trawl-admin"]), None)
        .await
        .unwrap();
    store.revoke_key(&created.info.prefix).await.unwrap();
    let err = store
        .retype_key(&created.info.prefix, PrincipalKind::Service)
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::KeyRevoked { .. }));
}

#[sqlx::test]
async fn ping_succeeds(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    store.ping().await.expect("ping");
}

// ---------------------------------------------------------------------------
// live-key lookup (ADR-0004: scheduler gating)
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn live_key_by_id_active(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let created = store
        .create_key(
            "live",
            PrincipalKind::Service,
            &names(&["trawl-admin"]),
            None,
        )
        .await
        .unwrap();

    let live = store
        .get_live_key_by_id(created.info.id)
        .await
        .expect("lookup")
        .expect("active key must be live");
    assert_eq!(live.id, created.info.id);
    assert_eq!(live.name, "live");
    assert!(live.has_app_permission("trawl", "query"));
}

#[sqlx::test]
async fn live_key_by_id_revoked_is_none(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let created = store
        .create_key(
            "revoked",
            PrincipalKind::Service,
            &names(&["trawl-admin"]),
            None,
        )
        .await
        .unwrap();
    store.revoke_key(&created.info.prefix).await.unwrap();

    let live = store.get_live_key_by_id(created.info.id).await.unwrap();
    assert!(live.is_none(), "revoked key must not be live");
}

#[sqlx::test]
async fn live_key_by_id_expired_is_none(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let created = store
        .create_key(
            "expiring-live",
            PrincipalKind::Service,
            &names(&["trawl-admin"]),
            Some(Duration::from_millis(50)),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;

    let live = store.get_live_key_by_id(created.info.id).await.unwrap();
    assert!(live.is_none(), "expired key must not be live");
}

#[sqlx::test]
async fn live_key_by_id_unknown_is_none(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let live = store.get_live_key_by_id(999_999).await.unwrap();
    assert!(live.is_none(), "unknown id must not be live");
}

#[sqlx::test]
async fn live_key_by_id_reflects_role_changes(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let created = store
        .create_key(
            "regrant",
            PrincipalKind::Service,
            &names(&["trawl-admin"]),
            None,
        )
        .await
        .unwrap();

    // Strip the role out-of-band — the live lookup must observe it.
    store
        .unassign_role(&created.info.prefix, "trawl-admin")
        .await
        .unwrap();
    let live = store
        .get_live_key_by_id(created.info.id)
        .await
        .unwrap()
        .expect("still active");
    assert!(!live.has_any_permission("trawl"), "roles must be fresh");

    // Re-assign a different role — fresh again.
    store
        .assign_role(&created.info.prefix, "trawl-analyst")
        .await
        .unwrap();
    let live = store
        .get_live_key_by_id(created.info.id)
        .await
        .unwrap()
        .expect("still active");
    assert_eq!(live.roles(), ["trawl-analyst"]);
    assert!(!live.has_app_permission("trawl", "server_manage"));
}

// ---------------------------------------------------------------------------
// connect() — URL-based construction for daemon consumers (ADR-0004)
// ---------------------------------------------------------------------------

/// `KeyStore::connect` validates both reachability and the migrated schema.
/// Its URL names the owned per-test database, never the `SQLx` admin database.
#[sqlx::test]
async fn connect_and_ping_via_url(pool: sqlx::PgPool) {
    use sqlx::ConnectOptions as _;
    let url = pool.connect_options().to_url_lossy();
    let store = KeyStore::connect(url.as_str()).await.expect("connect");
    store.ping().await.expect("ping");
    store.pool().close().await;
}

/// A raw `pool.begin()` dropped while Postgres' answer to its `BEGIN` is
/// on the way leaves its session inside a transaction sqlx never counted,
/// as when an admin operation's caller hangs up at that moment. The
/// keystore pool closes that session when it comes back instead of
/// reusing it: the lock the transaction took is gone with it, the pool's
/// next statement runs outside any transaction, and one warning names the
/// event. The answer is held back by a proxy until the `BEGIN` has been
/// dropped.
#[sqlx::test]
async fn a_keystore_connection_returned_inside_a_transaction_is_closed(pool: sqlx::PgPool) {
    use sqlx::ConnectOptions as _;
    use tracing_subscriber::layer::SubscriberExt as _;

    let capture = EventCapture::default();
    let event_types = Arc::clone(&capture.event_types);
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(capture));
    let proxy = answer_hold::Proxy::start(&pool.connect_options()).await;
    let url = proxy.options(&pool.connect_options()).to_url_lossy();
    let store = KeyStore::connect(url.as_str()).await.expect("connect");
    let through = store.pool().clone();

    proxy.hold_next(b"BEGIN");
    let begin = {
        let through = through.clone();
        tokio::spawn(async move {
            through
                .begin_with("BEGIN; SELECT pg_advisory_xact_lock(310)")
                .await
                .map(drop)
        })
    };
    proxy.holding().await;
    begin.abort();
    assert!(begin.await.unwrap_err().is_cancelled());
    proxy.release();

    let gone = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let held: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_locks
                                WHERE locktype = 'advisory' AND objid = 310
                                  AND database = (SELECT oid FROM pg_database
                                                  WHERE datname = current_database()))",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if !held {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(
        gone.is_ok(),
        "the dropped transaction's lock is still held: its session is back in the pool"
    );
    let settled = tokio::time::timeout(Duration::from_secs(10), async {
        while through.num_idle() != through.size() as usize {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(settled.is_ok(), "a connection never finished coming back");
    // One simple-protocol query: it starts the transaction it runs in
    // unless one is open already.
    let outside: bool = sqlx::Row::get(
        &sqlx::raw_sql("SELECT transaction_timestamp() = statement_timestamp()")
            .fetch_one(&through)
            .await
            .unwrap(),
        0,
    );
    assert!(
        outside,
        "the pool's next statement ran inside a transaction"
    );
    let warned = event_types
        .lock()
        .expect("event capture mutex")
        .iter()
        .filter(|event_type| *event_type == "pool_connection_returned_in_transaction")
        .count();
    assert_eq!(warned, 1, "one warning names the closed session");
}

/// The `last_used` of key `id`.
async fn last_used(pool: &sqlx::PgPool, id: i64) -> Option<chrono::DateTime<chrono::Utc>> {
    sqlx::query_scalar("SELECT last_used FROM api_keys WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A verification whose request is dropped while its transaction waits
/// for Postgres, as when a client resets its connection then, still runs
/// its transaction to the end: the transaction commits, its row lock goes
/// with the commit, and its session comes back to the pool outside any
/// transaction, so the pool's return check closes nothing and warns
/// nothing. The answer to the transaction's `BEGIN` is held back by a
/// proxy until the request has been dropped. The verification hits the
/// cache, the path that pays no argon2id and so is cheapest to repeat.
#[sqlx::test]
async fn a_verification_dropped_in_its_transaction_finishes_it_and_keeps_its_session(
    pool: sqlx::PgPool,
) {
    use sqlx::ConnectOptions as _;
    use tracing_subscriber::layer::SubscriberExt as _;

    let capture = EventCapture::default();
    let event_types = Arc::clone(&capture.event_types);
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(capture));
    let proxy = answer_hold::Proxy::start(&pool.connect_options()).await;
    let url = proxy.options(&pool.connect_options()).to_url_lossy();
    let store = KeyStore::connect(url.as_str()).await.expect("connect");
    let through = store.pool().clone();
    let created = store
        .create_key("reset", PrincipalKind::Service, &[], None)
        .await
        .unwrap();
    store.verify_key(&created.plaintext_token).await.unwrap();
    let used = last_used(&pool, created.info.id).await;

    proxy.hold_next(b"BEGIN");
    let verifying = {
        let store = store.clone();
        let token = created.plaintext_token.clone();
        tokio::spawn(async move { store.verify_key(&token).await.map(drop) })
    };
    proxy.holding().await;
    let session: i32 = sqlx::query_scalar(
        "SELECT pid FROM pg_stat_activity
         WHERE datname = current_database() AND state = 'idle in transaction'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    verifying.abort();
    assert!(verifying.await.unwrap_err().is_cancelled());
    proxy.release();

    let settled = tokio::time::timeout(Duration::from_secs(10), async {
        while through.num_idle() != through.size() as usize
            || last_used(&pool, created.info.id).await == used
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    let state: Option<String> =
        sqlx::query_scalar("SELECT state FROM pg_stat_activity WHERE pid = $1")
            .bind(session)
            .fetch_optional(&pool)
            .await
            .unwrap()
            .flatten();
    assert_eq!(
        state.as_deref(),
        Some("idle"),
        "the dropped verification's session did not come back to the pool"
    );
    let warned = event_types
        .lock()
        .expect("event capture mutex")
        .iter()
        .filter(|event_type| *event_type == "pool_connection_returned_in_transaction")
        .count();
    assert_eq!(warned, 0, "a session came back inside a transaction");
    assert!(
        settled.is_ok(),
        "the dropped verification's transaction never committed"
    );
    let mut probe = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM api_keys WHERE id = $1 FOR UPDATE NOWAIT")
        .bind(created.info.id)
        .execute(&mut *probe)
        .await
        .expect("the dropped verification's row lock is still held");
    probe.rollback().await.unwrap();
    // One simple-protocol query: it starts the transaction it runs in
    // unless one is open already.
    let outside: bool = sqlx::Row::get(
        &sqlx::raw_sql("SELECT transaction_timestamp() = statement_timestamp()")
            .fetch_one(&through)
            .await
            .unwrap(),
        0,
    );
    assert!(
        outside,
        "the pool's next statement ran inside a transaction"
    );
}

/// Dropped verifications leave no more transactions running behind them
/// than the store's pool has connections: a request takes its permit
/// before its transaction's task starts, and one dropped while it waits
/// for a permit starts nothing.
///
/// Four cache-hit verifications through a one-connection pool read their
/// key's row in turn while the test holds the connection (the pool serves
/// waiters first come, first served), so all four are past the read when
/// the first one's transaction starts and waits on the key's row lock,
/// which another session holds. All four requests are then dropped and
/// the lock released. A trigger counts the `last_used` updates: exactly
/// one transaction ran.
#[sqlx::test]
async fn dropped_verifications_leave_no_more_transactions_than_the_pool_has_connections(
    pool: sqlx::PgPool,
) {
    use sqlx::Connection as _;

    #[allow(clippy::disallowed_methods)] // a one-connection pool for the store
    let one = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let store = KeyStore::from_pool(one.clone());
    let created = store
        .create_key("reset", PrincipalKind::Service, &[], None)
        .await
        .unwrap();
    store.verify_key(&created.plaintext_token).await.unwrap();
    sqlx::raw_sql(
        "CREATE TABLE verification_runs (at timestamptz NOT NULL DEFAULT clock_timestamp());
         CREATE FUNCTION count_verification() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN INSERT INTO verification_runs DEFAULT VALUES; RETURN NEW; END $$;
         CREATE TRIGGER count_verification AFTER UPDATE OF last_used ON api_keys
         FOR EACH ROW EXECUTE FUNCTION count_verification();",
    )
    .execute(&pool)
    .await
    .unwrap();
    let runs = async || -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM verification_runs")
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let mut holder = sqlx::postgres::PgConnection::connect_with(&pool.connect_options())
        .await
        .unwrap();
    sqlx::query("BEGIN").execute(&mut holder).await.unwrap();
    sqlx::query("SELECT 1 FROM api_keys WHERE id = $1 FOR UPDATE")
        .bind(created.info.id)
        .execute(&mut holder)
        .await
        .unwrap();

    let gate = one.acquire().await.unwrap();
    let requests: Vec<_> = (0..4)
        .map(|_| {
            let store = store.clone();
            let token = created.plaintext_token.clone();
            tokio::spawn(async move { store.verify_key(&token).await.map(drop) })
        })
        .collect();
    // Each request now waits for the connection, in the order spawned.
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(gate);
    let waiting = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let blocked: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity
                 WHERE datname = current_database()
                   AND wait_event_type = 'Lock'
                   AND query LIKE 'UPDATE api_keys%'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if blocked == 1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(waiting.is_ok(), "no verification waited on the row lock");
    for request in &requests {
        request.abort();
    }
    for request in requests {
        assert!(request.await.unwrap_err().is_cancelled());
    }
    sqlx::query("COMMIT").execute(&mut holder).await.unwrap();

    let ran = tokio::time::timeout(Duration::from_secs(10), async {
        while runs().await == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(
        ran.is_ok(),
        "the first verification's transaction never finished"
    );
    // A transaction started past the permits would be waiting for the
    // pool's one connection, and runs as soon as the first one is done.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        runs().await,
        1,
        "a dropped request left a transaction behind past the permits"
    );
}

#[tokio::test]
async fn connect_malformed_url_errors() {
    // Malformed URL fails at parse time — no network round-trip, no timeout.
    let err = KeyStore::connect("not-a-postgres-url")
        .await
        .expect_err("must fail");
    assert!(matches!(err, AuthError::Database(_)), "got: {err:?}");
}

#[tokio::test]
async fn connect_unreachable_endpoint_errors() {
    // The endpoint must be unreachable in a way sqlx does NOT retry.
    // `PoolInner::connect` treats ECONNREFUSED as "the database is still
    // starting" and backs off until the acquire deadline, which
    // `KeyStore::connect` leaves at sqlx's default 30 seconds. So a dropped
    // port would make this test either slow or, worse, a pass by timeout:
    // the previous shape wrote `if let Ok(Ok(_)) = timeout(3s, ...)`, under
    // which Err(Elapsed) — a hang, the regression this test exists to catch
    // — sailed through as success.
    //
    // A listener we OWN for the whole test, accepting and immediately
    // closing, gives the postgres startup handshake an EOF instead. That is
    // not a retryable connect error, so the pool gives up at once, and
    // nothing can take the port from under us mid-test.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().expect("local_addr").port();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_in_thread = std::sync::Arc::clone(&stop);
    let closing = std::thread::spawn(move || {
        // One accept per connect attempt; the stream drops at the end of
        // each iteration, which closes it. The listener lives in this
        // thread, so the loop ends when the test sets `stop` and makes
        // one final connection to unblock `accept`.
        while let Ok((stream, _)) = listener.accept() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
            if stop_in_thread.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
        }
    });

    let url = format!("postgres://fleet:fleet@127.0.0.1:{port}/fleet_test");
    let started = std::time::Instant::now();
    let outcome = tokio::time::timeout(BOUNDED_CONNECT, KeyStore::connect(&url)).await;
    let elapsed = started.elapsed();

    match outcome {
        Ok(Err(e)) => {
            assert!(
                matches!(e, AuthError::Database(_)),
                "unreachable endpoint must surface as a database error: {e:?}"
            );
        }
        Ok(Ok(_)) => panic!("connect must not succeed against an unreachable endpoint"),
        Err(tokio::time::error::Elapsed { .. }) => panic!(
            "KeyStore::connect did not fail within {BOUNDED_CONNECT:?} against a \
             socket that closes every connection. Either the pool started \
             retrying an error it used to give up on, or connect grew an \
             unbounded wait: trawld boots through this call, so a hang here \
             is a daemon that never starts and never says why"
        ),
    }
    assert!(
        elapsed < BOUNDED_CONNECT,
        "connect took {elapsed:?}, at the {BOUNDED_CONNECT:?} bound"
    );

    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = std::net::TcpStream::connect(("127.0.0.1", port)); // unblock accept
    let _ = closing.join();
}

/// How long an unreachable-endpoint connect may take before we call it a
/// hang. A closed socket answers in milliseconds; two seconds is slack for
/// a loaded runner, not a budget the healthy path spends.
const BOUNDED_CONNECT: Duration = Duration::from_secs(2);

// One sync sanity test to confirm imports compile without DB.
#[test]
fn validation_reexported_works() {
    assert!(validate_app_namespace("trawl").is_ok());
    assert!(validate_app_namespace("BAD").is_err());
    let t = token::generate_token();
    assert!(t.plaintext.starts_with("flt_"));
}

// ---------------------------------------------------------------------------
// A warm cache never outranks the database (#304). The cache only records
// that argon2id accepted a plaintext; activity, expiry, roles and the
// stored hash are read from Postgres on every verify.
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn warm_cache_then_expiry_refuses_the_next_verify(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let created = store
        .create_key(
            "short-lived",
            PrincipalKind::Human,
            &names(&["trawl-admin"]),
            Some(Duration::from_secs(1)),
        )
        .await
        .unwrap();
    // Warm the cache while the key is live.
    store
        .verify_key(&created.plaintext_token)
        .await
        .expect("a live key verifies");
    assert_eq!(store.cache_stats().entries, 1);

    tokio::time::sleep(Duration::from_millis(1300)).await;
    let result = store.verify_key(&created.plaintext_token).await;
    assert!(
        matches!(result, Err(AuthError::InvalidKey(_))),
        "a cached entry does not outlive the key's expiry"
    );
    assert_eq!(
        store.cache_stats().entries,
        1,
        "the database refused the key; the entry is still cached"
    );
}

#[sqlx::test]
async fn warm_cache_then_role_change_shows_on_the_next_verify(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let created = store
        .create_key(
            "promoted",
            PrincipalKind::Human,
            &names(&["trawl-analyst"]),
            None,
        )
        .await
        .unwrap();
    let warm = store.verify_key(&created.plaintext_token).await.unwrap();
    assert_eq!(warm.roles(), ["trawl-analyst"]);
    assert_eq!(store.cache_stats().entries, 1);

    store
        .assign_role(&created.info.prefix, "trawl-admin")
        .await
        .unwrap();
    let promoted = store.verify_key(&created.plaintext_token).await.unwrap();
    assert_eq!(promoted.roles(), ["trawl-admin", "trawl-analyst"]);

    store
        .unassign_role(&created.info.prefix, "trawl-analyst")
        .await
        .unwrap();
    let demoted = store.verify_key(&created.plaintext_token).await.unwrap();
    assert_eq!(demoted.roles(), ["trawl-admin"]);
    assert_eq!(
        store.cache_stats().entries,
        1,
        "role changes never touch the cache; each verify read them from the database"
    );
}

#[sqlx::test]
async fn warm_cache_then_hash_rotation_refuses_the_old_token(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;

    let rotated = store
        .create_key(
            "rotated",
            PrincipalKind::Human,
            &names(&["trawl-admin"]),
            None,
        )
        .await
        .unwrap();
    let donor = store
        .create_key(
            "donor",
            PrincipalKind::Human,
            &names(&["trawl-admin"]),
            None,
        )
        .await
        .unwrap();
    store
        .verify_key(&rotated.plaintext_token)
        .await
        .expect("the old token verifies and warms the cache");
    assert_eq!(store.cache_stats().entries, 1);

    // Rotate the stored hash out from under the cached entry. The donor's
    // hash is a valid PHC string whose plaintext is not the rotated token.
    sqlx::query(
        "UPDATE api_keys
         SET hash = (SELECT hash FROM api_keys WHERE id = $2)
         WHERE id = $1",
    )
    .bind(rotated.info.id)
    .bind(donor.info.id)
    .execute(store.pool())
    .await
    .unwrap();

    // The cache key binds the stored hash, so the warm entry is unreachable
    // and argon2id refuses the old plaintext against the new hash.
    let result = store.verify_key(&rotated.plaintext_token).await;
    assert!(matches!(result, Err(AuthError::InvalidKey(_))));
    assert_eq!(
        store.cache_stats().entries,
        1,
        "a refused KDF run inserts nothing"
    );
    // The untouched key is unaffected.
    store
        .verify_key(&donor.plaintext_token)
        .await
        .expect("the donor still verifies");
}

/// Start a verification of `token` while another transaction holds the
/// key's row, so the verification's `last_used` UPDATE waits on the lock
/// after its SELECT read the old hash and after argon2id, if it ran,
/// accepted the old plaintext. Rotate the hash to `donor_id`'s in the
/// holding transaction, commit, and return what the verification decided
/// once the lock let it continue.
async fn verify_while_the_hash_rotates(
    store: &KeyStore,
    token: &str,
    key_id: i64,
    donor_id: i64,
) -> Result<fleet_auth::VerifiedKey, AuthError> {
    let mut holder = store.pool().begin().await.unwrap();
    sqlx::query("SELECT id FROM api_keys WHERE id = $1 FOR UPDATE")
        .bind(key_id)
        .execute(&mut *holder)
        .await
        .unwrap();

    let verifying = tokio::spawn({
        let store = store.clone();
        let token = token.to_owned();
        async move { store.verify_key(&token).await }
    });

    // Wait until the verification's UPDATE is blocked on the held row:
    // its SELECT and any KDF are behind it by then.
    let started = std::time::Instant::now();
    loop {
        let blocked: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
             WHERE datname = current_database()
               AND wait_event_type = 'Lock'
               AND query LIKE 'UPDATE api_keys%'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        if blocked >= 1 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the verification never reached its last_used UPDATE"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    sqlx::query(
        "UPDATE api_keys
         SET hash = (SELECT hash FROM api_keys WHERE id = $2)
         WHERE id = $1",
    )
    .bind(key_id)
    .bind(donor_id)
    .execute(&mut *holder)
    .await
    .unwrap();
    holder.commit().await.unwrap();

    verifying
        .await
        .expect("the verification task ran to its end")
}

/// A rotation that commits after a verification read the old hash and
/// argon2id accepted the old plaintext, but before its `last_used` UPDATE
/// ran, refuses that verification: the UPDATE requires the hash it was
/// verified against, so the old token never authenticates a rotated key.
#[sqlx::test]
async fn concurrent_hash_rotation_refuses_the_old_token_on_a_cache_miss(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;
    let rotated = store
        .create_key(
            "rotated",
            PrincipalKind::Human,
            &names(&["trawl-admin"]),
            None,
        )
        .await
        .unwrap();
    let donor = store
        .create_key(
            "donor",
            PrincipalKind::Human,
            &names(&["trawl-admin"]),
            None,
        )
        .await
        .unwrap();
    assert_eq!(store.cache_stats().entries, 0, "nothing is cached yet");

    let result = verify_while_the_hash_rotates(
        &store,
        &rotated.plaintext_token,
        rotated.info.id,
        donor.info.id,
    )
    .await;
    assert!(
        matches!(result, Err(AuthError::InvalidKey(_))),
        "the old token authenticated a rotated key: {result:?}"
    );
    assert_eq!(
        store.cache_stats().entries,
        0,
        "a refused verification inserts nothing"
    );
    store
        .verify_key(&donor.plaintext_token)
        .await
        .expect("the donor still verifies");
}

/// The same race on a cache hit: the warm entry skips argon2id, and the
/// UPDATE still refuses once the hash it was looked up under is gone.
#[sqlx::test]
async fn concurrent_hash_rotation_refuses_the_old_token_on_a_cache_hit(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);
    seed_roles(&store).await;
    let rotated = store
        .create_key(
            "rotated",
            PrincipalKind::Human,
            &names(&["trawl-admin"]),
            None,
        )
        .await
        .unwrap();
    let donor = store
        .create_key(
            "donor",
            PrincipalKind::Human,
            &names(&["trawl-admin"]),
            None,
        )
        .await
        .unwrap();
    store
        .verify_key(&rotated.plaintext_token)
        .await
        .expect("the old token verifies and warms the cache");
    assert_eq!(store.cache_stats().entries, 1);

    let result = verify_while_the_hash_rotates(
        &store,
        &rotated.plaintext_token,
        rotated.info.id,
        donor.info.id,
    )
    .await;
    assert!(
        matches!(result, Err(AuthError::InvalidKey(_))),
        "the old token authenticated a rotated key from the cache: {result:?}"
    );
    assert_eq!(
        store.cache_stats().entries,
        1,
        "the stale entry stays, bound to a hash no row has"
    );
    store
        .verify_key(&donor.plaintext_token)
        .await
        .expect("the donor still verifies");
}

#[sqlx::test]
async fn revocation_through_a_second_store_refuses_the_first_stores_warm_key(pool: sqlx::PgPool) {
    // Two processes sharing one database each hold their own cache. A
    // revocation through either refuses the key everywhere on its next use.
    let first = KeyStore::from_pool(pool.clone());
    let second = KeyStore::from_pool(pool);
    seed_roles(&first).await;

    let created = first
        .create_key(
            "shared-db",
            PrincipalKind::Human,
            &names(&["trawl-admin"]),
            None,
        )
        .await
        .unwrap();
    first
        .verify_key(&created.plaintext_token)
        .await
        .expect("warm the first store");
    second
        .verify_key(&created.plaintext_token)
        .await
        .expect("warm the second store");
    assert_eq!(first.cache_stats().entries, 1);
    assert_eq!(second.cache_stats().entries, 1);

    second.revoke_key(&created.info.prefix).await.unwrap();

    let through_first = first.verify_key(&created.plaintext_token).await;
    assert!(
        matches!(through_first, Err(AuthError::InvalidKey(_))),
        "the first store's warm entry does not outrank the revocation"
    );
    let through_second = second.verify_key(&created.plaintext_token).await;
    assert!(matches!(through_second, Err(AuthError::InvalidKey(_))));
    assert_eq!(
        first.cache_stats().entries,
        1,
        "the database refused the key; the first store's entry is still cached"
    );
}
