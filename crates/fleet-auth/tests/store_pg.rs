// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres integration tests for fleet-auth's `KeyStore`.
//!
//! Fixture and `pg_test!` macro live in [`common`] so middleware + handler
//! integration tests can reuse them.

#![cfg(feature = "keystore")]

use std::time::Duration;

use fleet_auth::{
    AuthError, KeyStore, PrincipalKind, RoleAssignment, token, validate_app_namespace,
};

fn trawl_admin() -> Vec<RoleAssignment> {
    vec![RoleAssignment {
        app: "trawl".into(),
        role: "admin".into(),
    }]
}

// ---------------------------------------------------------------------------
// lifecycle tests
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn create_and_verify_roundtrip(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let created = store
        .create_key("test", PrincipalKind::Human, &trawl_admin(), None)
        .await
        .expect("create");
    assert!(created.plaintext_token.starts_with("flt_"));

    let verified = store
        .verify_key(&created.plaintext_token)
        .await
        .expect("verify");
    assert_eq!(verified.name, "test");
    assert_eq!(verified.kind, PrincipalKind::Human);
    assert_eq!(verified.role_for("trawl"), Some("admin"));
}

#[sqlx::test]
async fn create_with_multiple_app_grants(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let grants = vec![
        RoleAssignment {
            app: "trawl".into(),
            role: "analyst".into(),
        },
        RoleAssignment {
            app: "coastwatch".into(),
            role: "siem_consumer".into(),
        },
    ];
    let created = store
        .create_key("multi", PrincipalKind::Service, &grants, None)
        .await
        .expect("create");

    let verified = store.verify_key(&created.plaintext_token).await.unwrap();
    assert_eq!(verified.kind, PrincipalKind::Service);
    assert_eq!(verified.assignments.len(), 2);
    assert_eq!(verified.role_for("trawl"), Some("analyst"));
    assert_eq!(verified.role_for("coastwatch"), Some("siem_consumer"));
}

#[sqlx::test]
async fn duplicate_app_in_batch_rejected(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let err = store
        .create_key(
            "dup",
            PrincipalKind::Human,
            &[
                RoleAssignment {
                    app: "trawl".into(),
                    role: "admin".into(),
                },
                RoleAssignment {
                    app: "trawl".into(),
                    role: "reader".into(),
                },
            ],
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::GrantExists { .. }));
}

#[sqlx::test]
async fn empty_grants_create_then_verify(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let created = store
        .create_key("empty", PrincipalKind::Service, &[], None)
        .await
        .expect("create");
    let verified = store.verify_key(&created.plaintext_token).await.unwrap();
    assert!(verified.assignments.is_empty());
}

#[sqlx::test]
async fn create_key_with_expiry_in_future(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let created = store
        .create_key(
            "expiring",
            PrincipalKind::Human,
            &trawl_admin(),
            Some(Duration::from_hours(24)),
        )
        .await
        .expect("create");
    assert!(created.info.expires_at.is_some());
    let verified = store.verify_key(&created.plaintext_token).await.unwrap();
    assert_eq!(verified.name, "expiring");
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

    let created = store
        .create_key("legit", PrincipalKind::Human, &trawl_admin(), None)
        .await
        .unwrap();
    // Same prefix, different body — should NOT verify (this is the security
    // regression test for the cache key shape — if cache were
    // (prefix, hash) only, this would falsely succeed after a successful
    // verify of the legit token).
    store.verify_key(&created.plaintext_token).await.unwrap();
    let prefix_only = &created.plaintext_token[..12]; // "flt_" + 8
    let tampered = format!("{prefix_only}AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA0");
    let result = store.verify_key(&tampered).await;
    assert!(matches!(result, Err(AuthError::InvalidKey(_))));
}

#[sqlx::test]
async fn cache_hit_skips_kdf_but_updates_last_used(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let created = store
        .create_key("cacheable", PrincipalKind::Human, &trawl_admin(), None)
        .await
        .unwrap();

    let before = store.cache_stats().entries;
    store.verify_key(&created.plaintext_token).await.unwrap();
    let after_first = store.cache_stats().entries;
    assert_eq!(after_first, before + 1, "first verify populates cache");

    // Capture last_used from after the first verify.
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

    let created = store
        .create_key("revokable", PrincipalKind::Human, &trawl_admin(), None)
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

    // Can't backdate expires_at directly — the api_keys_expiry_after_create
    // CHECK constraint refuses any expires_at <= created_at. Instead, create
    // with a near-zero TTL and sleep past it before verifying.
    let created = store
        .create_key(
            "doomed",
            PrincipalKind::Human,
            &trawl_admin(),
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

    let created = store
        .create_key("racy", PrincipalKind::Human, &trawl_admin(), None)
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

    let created = store
        .create_key("concurrent", PrincipalKind::Human, &trawl_admin(), None)
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

#[sqlx::test]
async fn revoke_assignment_waits_for_key_row_lock(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let created = store
        .create_key(
            "lock-step",
            PrincipalKind::Human,
            &[
                RoleAssignment {
                    app: "trawl".into(),
                    role: "admin".into(),
                },
                RoleAssignment {
                    app: "coastwatch".into(),
                    role: "siem_consumer".into(),
                },
            ],
            None,
        )
        .await
        .unwrap();

    // `verify_key` now holds this same row lock while it fetches
    // assignments. Holding it manually gives the regression test a
    // deterministic interleaving instead of hoping the scheduler lands
    // inside a microsecond race window.
    let mut tx = store.pool().begin().await.unwrap();
    sqlx::query("SELECT id FROM api_keys WHERE id = $1 FOR UPDATE")
        .bind(created.info.id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();

    let revoke_store = store.clone();
    let prefix = created.info.prefix.clone();
    let revoke =
        tokio::spawn(async move { revoke_store.revoke_assignment(&prefix, "coastwatch").await });

    assert!(
        tokio::time::timeout(Duration::from_millis(100), async {
            while !revoke.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_err(),
        "revoke_assignment must wait for the key row lock"
    );

    tx.commit().await.unwrap();
    revoke.await.unwrap().unwrap();

    let verified = store.verify_key(&created.plaintext_token).await.unwrap();
    assert_eq!(verified.role_for("coastwatch"), None);
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

    let created = store
        .create_key("twice", PrincipalKind::Human, &trawl_admin(), None)
        .await
        .unwrap();
    store.revoke_key(&created.info.prefix).await.unwrap();
    let result = store.revoke_key(&created.info.prefix).await;
    assert!(matches!(result, Err(AuthError::KeyRevoked { .. })));
}

#[sqlx::test]
async fn list_keys_active_only_filters(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let keep = store
        .create_key("keep", PrincipalKind::Human, &trawl_admin(), None)
        .await
        .unwrap();
    let drop_me = store
        .create_key("drop", PrincipalKind::Human, &trawl_admin(), None)
        .await
        .unwrap();
    store.revoke_key(&drop_me.info.prefix).await.unwrap();

    let active = store.list_keys(true).await.unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].prefix, keep.info.prefix);

    let all = store.list_keys(false).await.unwrap();
    assert_eq!(all.len(), 2);
}

#[sqlx::test]
async fn grant_and_revoke_assignment_roundtrip(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let created = store
        .create_key("shared", PrincipalKind::Service, &trawl_admin(), None)
        .await
        .unwrap();
    store
        .grant_assignment(
            &created.info.prefix,
            &RoleAssignment {
                app: "coastwatch".into(),
                role: "siem_consumer".into(),
            },
        )
        .await
        .unwrap();

    let after = store.list_assignments(&created.info.prefix).await.unwrap();
    assert_eq!(after.len(), 2);

    store
        .revoke_assignment(&created.info.prefix, "coastwatch")
        .await
        .unwrap();
    let after_revoke = store.list_assignments(&created.info.prefix).await.unwrap();
    assert_eq!(after_revoke.len(), 1);
}

#[sqlx::test]
async fn grant_rejects_duplicate_app(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let created = store
        .create_key("k", PrincipalKind::Human, &trawl_admin(), None)
        .await
        .unwrap();
    let err = store
        .grant_assignment(
            &created.info.prefix,
            &RoleAssignment {
                app: "trawl".into(),
                role: "reader".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::GrantExists { .. }));
}

#[sqlx::test]
async fn revoke_missing_grant_errors(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let created = store
        .create_key("k", PrincipalKind::Human, &trawl_admin(), None)
        .await
        .unwrap();
    let err = store
        .revoke_assignment(&created.info.prefix, "nonexistent")
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::GrantNotFound { .. }));
}

#[sqlx::test]
async fn retype_key_succeeds(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let created = store
        .create_key("morphing", PrincipalKind::Human, &trawl_admin(), None)
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

    let created = store
        .create_key("dead", PrincipalKind::Human, &trawl_admin(), None)
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

    let created = store
        .create_key("live", PrincipalKind::Service, &trawl_admin(), None)
        .await
        .unwrap();

    let live = store
        .get_live_key_by_id(created.info.id)
        .await
        .expect("lookup")
        .expect("active key must be live");
    assert_eq!(live.id, created.info.id);
    assert_eq!(live.name, "live");
    assert_eq!(live.role_for("trawl"), Some("admin"));
}

#[sqlx::test]
async fn live_key_by_id_revoked_is_none(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let created = store
        .create_key("revoked", PrincipalKind::Service, &trawl_admin(), None)
        .await
        .unwrap();
    store.revoke_key(&created.info.prefix).await.unwrap();

    let live = store.get_live_key_by_id(created.info.id).await.unwrap();
    assert!(live.is_none(), "revoked key must not be live");
}

#[sqlx::test]
async fn live_key_by_id_expired_is_none(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let created = store
        .create_key(
            "expiring-live",
            PrincipalKind::Service,
            &trawl_admin(),
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
async fn live_key_by_id_reflects_grant_changes(pool: sqlx::PgPool) {
    let store = KeyStore::from_pool(pool);

    let created = store
        .create_key("regrant", PrincipalKind::Service, &trawl_admin(), None)
        .await
        .unwrap();

    // Strip the trawl grant out-of-band — the live lookup must observe it.
    store
        .revoke_assignment(&created.info.prefix, "trawl")
        .await
        .unwrap();
    let live = store
        .get_live_key_by_id(created.info.id)
        .await
        .unwrap()
        .expect("still active");
    assert_eq!(live.role_for("trawl"), None, "assignments must be fresh");

    // Re-grant with a different role — fresh again.
    store
        .grant_assignment(
            &created.info.prefix,
            &RoleAssignment {
                app: "trawl".into(),
                role: "reader".into(),
            },
        )
        .await
        .unwrap();
    let live = store
        .get_live_key_by_id(created.info.id)
        .await
        .unwrap()
        .expect("still active");
    assert_eq!(live.role_for("trawl"), Some("reader"));
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

// One sync sanity test to confirm imports compile without DB.
#[test]
fn validation_reexported_works() {
    assert!(validate_app_namespace("trawl").is_ok());
    assert!(validate_app_namespace("BAD").is_err());
    // and token module is reachable
    let t = token::generate_token();
    assert!(t.plaintext.starts_with("flt_"));
}
