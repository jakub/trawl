// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Integration coverage for `fleet-admin keys` against a real Postgres.
//!
//! Calls fleet-auth's `KeyStore` directly (the same API surface the CLI
//! drives) rather than spawning the `fleet-admin` binary per test. Faster,
//! and the binary's command functions are thin enough that they don't
//! contribute additional behaviour worth re-validating from a subprocess.

mod common;

use std::time::Duration;

use fleet_admin::commands::keys::{self, KeyPrefix};
use fleet_admin::error::AdminError;
use fleet_auth::{AuthError, PrincipalKind, RoleAssignment};

fn trawl_admin() -> RoleAssignment {
    RoleAssignment {
        app: "trawl".into(),
        role: "admin".into(),
    }
}

fn coastwatch_consumer() -> RoleAssignment {
    RoleAssignment {
        app: "coastwatch".into(),
        role: "siem_consumer".into(),
    }
}

#[sqlx::test(migrations = false)]
async fn create_emits_flt_prefix_and_persists(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;

    let created = store
        .create_key("svc", PrincipalKind::Service, &[trawl_admin()], None)
        .await
        .expect("create");

    assert!(
        created.plaintext_token.starts_with("flt_"),
        "expected flt_ prefix, got {:?}",
        *created.plaintext_token
    );
    assert!(created.info.active);
    assert!(created.info.revoked_at.is_none());

    let listed = store.list_keys(true).await.expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].prefix, created.info.prefix);
    assert_eq!(listed[0].name, "svc");
    assert_eq!(listed[0].kind, PrincipalKind::Service);
    assert_eq!(listed[0].assignments.len(), 1);
}

#[sqlx::test(migrations = false)]
async fn list_all_includes_revoked(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;

    let alive = store
        .create_key("alive", PrincipalKind::Human, &[trawl_admin()], None)
        .await
        .unwrap();
    let dead = store
        .create_key("dead", PrincipalKind::Human, &[trawl_admin()], None)
        .await
        .unwrap();
    store.revoke_key(&dead.info.prefix).await.unwrap();

    let active_only = store.list_keys(true).await.unwrap();
    assert_eq!(active_only.len(), 1);
    assert_eq!(active_only[0].prefix, alive.info.prefix);

    let all = store.list_keys(false).await.unwrap();
    assert_eq!(all.len(), 2);
    let revoked = all.iter().find(|k| k.prefix == dead.info.prefix).unwrap();
    assert!(!revoked.active);
    assert!(revoked.revoked_at.is_some());
}

#[sqlx::test(migrations = false)]
async fn revoke_sets_active_false_and_revoked_at(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;

    let created = store
        .create_key("target", PrincipalKind::Human, &[], None)
        .await
        .unwrap();

    let info = store
        .revoke_key(&created.info.prefix)
        .await
        .expect("revoke");
    assert!(!info.active);
    assert!(info.revoked_at.is_some(), "revoked_at must be populated");

    let err = store
        .verify_key(&created.plaintext_token)
        .await
        .unwrap_err();
    assert!(matches!(err, AuthError::InvalidKey(_)));
}

#[sqlx::test(migrations = false)]
async fn grant_adds_assignment_and_rejects_duplicate(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;

    let created = store
        .create_key("grantee", PrincipalKind::Service, &[], None)
        .await
        .unwrap();

    store
        .grant_assignment(&created.info.prefix, &trawl_admin())
        .await
        .expect("first grant");

    // Same (key, app) again → GrantExists, NOT silent overwrite.
    let err = store
        .grant_assignment(&created.info.prefix, &trawl_admin())
        .await
        .unwrap_err();
    assert!(
        matches!(err, AuthError::GrantExists { ref app, .. } if app == "trawl"),
        "expected GrantExists for app=trawl, got {err:?}"
    );

    store
        .grant_assignment(&created.info.prefix, &coastwatch_consumer())
        .await
        .expect("grant on different app");

    let info = store.get_key_by_prefix(&created.info.prefix).await.unwrap();
    assert_eq!(info.assignments.len(), 2);
}

#[sqlx::test(migrations = false)]
async fn grant_on_unknown_prefix_returns_key_not_found(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;

    let err = store
        .grant_assignment("ghostpfx", &trawl_admin())
        .await
        .unwrap_err();
    assert!(
        matches!(err, AuthError::KeyNotFound { ref prefix } if prefix == "ghostpfx"),
        "expected KeyNotFound, got {err:?}"
    );
}

#[sqlx::test(migrations = false)]
async fn revoke_unknown_prefix_returns_key_not_found(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;

    let err = store.revoke_key("ghostpfx").await.unwrap_err();
    assert!(
        matches!(err, AuthError::KeyNotFound { ref prefix } if prefix == "ghostpfx"),
        "expected KeyNotFound, got {err:?}"
    );
}

#[sqlx::test(migrations = false)]
async fn create_with_expires_persists_expires_at(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;

    let ninety_days = Duration::from_secs(90 * 86_400);
    let before = chrono::Utc::now();
    keys::create(
        &store,
        "exp",
        PrincipalKind::Service,
        &[trawl_admin()],
        Some(ninety_days),
    )
    .await
    .expect("create");

    let listed = store.list_keys(true).await.expect("list");
    assert_eq!(listed.len(), 1);
    let expires_at = listed[0].expires_at.expect("expires_at must be populated");

    let expected = before + chrono::Duration::seconds(90 * 86_400);
    let delta = (expires_at - expected).num_seconds().abs();
    assert!(
        delta < 5,
        "expires_at drift {delta}s vs expected {expected}, got {expires_at}"
    );
}

#[sqlx::test(migrations = false)]
async fn revoke_twice_returns_already_revoked(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;

    let created = store
        .create_key("twice", PrincipalKind::Human, &[trawl_admin()], None)
        .await
        .unwrap();
    let prefix = KeyPrefix::parse(&created.info.prefix).expect("parse prefix");

    keys::revoke(&store, &prefix, true)
        .await
        .expect("first revoke");

    let err = keys::revoke(&store, &prefix, true)
        .await
        .expect_err("second revoke must error");
    assert!(
        matches!(err, AdminError::AlreadyRevoked { ref prefix, .. } if prefix == &created.info.prefix),
        "expected AlreadyRevoked, got {err:?}"
    );
}

#[sqlx::test(migrations = false)]
async fn revoke_grant_removes_only_named_app(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;

    let created = store
        .create_key(
            "multi",
            PrincipalKind::Service,
            &[trawl_admin(), coastwatch_consumer()],
            None,
        )
        .await
        .unwrap();
    let prefix = KeyPrefix::parse(&created.info.prefix).expect("parse prefix");

    keys::revoke_grant(&store, &prefix, "trawl", true)
        .await
        .expect("revoke grant");

    let info = store.get_key_by_prefix(&created.info.prefix).await.unwrap();
    assert_eq!(info.assignments.len(), 1, "other grant must survive");
    assert_eq!(info.assignments[0].app, "coastwatch");
}

#[sqlx::test(migrations = false)]
async fn revoke_grant_missing_app_returns_grant_not_found(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;

    let created = store
        .create_key(
            "bare",
            PrincipalKind::Service,
            &[coastwatch_consumer()],
            None,
        )
        .await
        .unwrap();
    let prefix = KeyPrefix::parse(&created.info.prefix).expect("parse prefix");

    let err = keys::revoke_grant(&store, &prefix, "trawl", true)
        .await
        .expect_err("missing grant must error, not silently succeed");
    assert!(
        matches!(
            err,
            AdminError::Auth(AuthError::GrantNotFound { ref app, .. }) if app == "trawl"
        ),
        "expected GrantNotFound for app=trawl, got {err:?}"
    );
}

#[sqlx::test(migrations = false)]
async fn revoke_grant_unknown_prefix_returns_key_not_found(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;

    let prefix = KeyPrefix::parse("ghostpfx").expect("parse prefix");
    let err = keys::revoke_grant(&store, &prefix, "trawl", true)
        .await
        .expect_err("unknown prefix must error");
    assert!(
        matches!(
            err,
            AdminError::Auth(AuthError::KeyNotFound { ref prefix }) if prefix == "ghostpfx"
        ),
        "expected KeyNotFound, got {err:?}"
    );
}

#[sqlx::test(migrations = false)]
async fn retype_flips_kind_both_directions(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;

    let created = store
        .create_key("flip", PrincipalKind::Human, &[trawl_admin()], None)
        .await
        .unwrap();
    let prefix = KeyPrefix::parse(&created.info.prefix).expect("parse prefix");

    keys::retype(&store, &prefix, PrincipalKind::Service)
        .await
        .expect("human -> service");
    let info = store.get_key_by_prefix(&created.info.prefix).await.unwrap();
    assert_eq!(info.kind, PrincipalKind::Service, "must persist");

    keys::retype(&store, &prefix, PrincipalKind::Human)
        .await
        .expect("service -> human");
    let info = store.get_key_by_prefix(&created.info.prefix).await.unwrap();
    assert_eq!(info.kind, PrincipalKind::Human, "must flip back");
}

#[sqlx::test(migrations = false)]
async fn retype_revoked_key_returns_key_revoked(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;

    let created = store
        .create_key("dead", PrincipalKind::Human, &[], None)
        .await
        .unwrap();
    store.revoke_key(&created.info.prefix).await.unwrap();
    let prefix = KeyPrefix::parse(&created.info.prefix).expect("parse prefix");

    let err = keys::retype(&store, &prefix, PrincipalKind::Service)
        .await
        .expect_err("revoked key must be refused");
    assert!(
        matches!(
            err,
            AdminError::Auth(AuthError::KeyRevoked { ref prefix }) if prefix == &created.info.prefix
        ),
        "expected KeyRevoked, got {err:?}"
    );
}

#[sqlx::test(migrations = false)]
async fn retype_unknown_prefix_returns_key_not_found(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;

    let prefix = KeyPrefix::parse("ghostpfx").expect("parse prefix");
    let err = keys::retype(&store, &prefix, PrincipalKind::Service)
        .await
        .expect_err("unknown prefix must error");
    assert!(
        matches!(
            err,
            AdminError::Auth(AuthError::KeyNotFound { ref prefix }) if prefix == "ghostpfx"
        ),
        "expected KeyNotFound, got {err:?}"
    );
}

#[sqlx::test(migrations = false)]
async fn create_duplicate_name_is_allowed(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;

    // Pin the schema contract — name is NOT unique, two keys can share
    // a label (operators distinguish them by prefix).
    store
        .create_key("dup", PrincipalKind::Service, &[], None)
        .await
        .expect("first");
    store
        .create_key("dup", PrincipalKind::Service, &[], None)
        .await
        .expect("second");
    let all = store.list_keys(true).await.unwrap();
    assert_eq!(all.iter().filter(|k| k.name == "dup").count(), 2);
}
