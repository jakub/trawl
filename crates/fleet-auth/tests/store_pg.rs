// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres integration tests for fleet-auth's `KeyStore` under the
//! roles-as-data model (ADR-0006).

#![cfg(feature = "keystore")]

use std::time::Duration;

use fleet_auth::{
    AuthError, KeyStore, PrincipalKind, RolePermission, token, validate_app_namespace,
};

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

    assert!(
        tokio::time::timeout(Duration::from_millis(100), async {
            while !unassign.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_err(),
        "unassign_role must wait for the key row lock"
    );

    tx.commit().await.unwrap();
    unassign.await.unwrap().unwrap();

    let verified = store.verify_key(&created.plaintext_token).await.unwrap();
    assert_eq!(verified.roles(), ["trawl-admin"]);
    assert!(!verified.has_any_permission("coastwatch"));
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

/// `KeyStore::connect` establishes a pool from a database URL and validates
/// connectivity eagerly (trawld fails fast at startup on a dead backend).
#[sqlx::test(migrations = false)]
async fn connect_and_ping_via_url(_pool: sqlx::PgPool) {
    // DATABASE_URL is guaranteed set here — #[sqlx::test] enforces it
    // loudly. Ping only touches SELECT 1, so the admin database is fine.
    let url = std::env::var("DATABASE_URL").expect("#[sqlx::test] enforces DATABASE_URL");
    let store = KeyStore::connect(&url).await.expect("connect");
    store.ping().await.expect("ping");
    // Close eagerly so nextest doesn't flag the lazy pool teardown as a leak.
    store.pool().close().await;
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
    let closing = std::thread::spawn(move || {
        // One accept per connect attempt; the stream drops at the end of
        // each iteration, which closes it. The loop ends when the test
        // drops the listener half by returning (accept then errors).
        while let Ok((stream, _)) = listener.accept() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
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

    drop(closing); // the accept loop exits when the process does
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
