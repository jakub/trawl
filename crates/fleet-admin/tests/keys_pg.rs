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

use fleet_admin::commands::keys::{self, KeyPrefix, RoleName};
use fleet_admin::error::AdminError;
use fleet_auth::{AuthError, KeyStore, PrincipalKind, RolePermission};

fn rp(app: &str, permission: &str) -> RolePermission {
    RolePermission {
        app: app.into(),
        permission: permission.into(),
    }
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

/// Seed the converted-shape roles the key tests hand out.
async fn seed_roles(store: &KeyStore) {
    store
        .create_role(
            "trawl-admin",
            None,
            &[rp("trawl", "query"), rp("trawl", "server_manage")],
        )
        .await
        .expect("seed trawl-admin");
    store
        .create_role(
            "coastwatch-siem_consumer",
            None,
            &[rp("coastwatch", "ioc_exports_read")],
        )
        .await
        .expect("seed coastwatch-siem_consumer");
}

#[sqlx::test(migrations = false)]
async fn create_emits_flt_prefix_and_persists(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;
    seed_roles(&store).await;

    let created = store
        .create_key(
            "svc",
            PrincipalKind::Service,
            &names(&["trawl-admin"]),
            None,
        )
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
    assert_eq!(listed[0].roles, ["trawl-admin"]);
}

#[sqlx::test(migrations = false)]
async fn create_with_unknown_role_is_refused(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;

    let err = keys::create(
        &store,
        "typo",
        PrincipalKind::Service,
        &names(&["trawl-adnim"]),
        None,
    )
    .await
    .expect_err("unknown role must be refused, not silently minted");
    assert!(
        matches!(
            err,
            AdminError::Auth(AuthError::RoleNotFound { ref name }) if name == "trawl-adnim"
        ),
        "expected RoleNotFound, got {err:?}"
    );
    assert!(store.list_keys(false).await.unwrap().is_empty());
}

#[sqlx::test(migrations = false)]
async fn list_all_includes_revoked(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;
    seed_roles(&store).await;

    let alive = store
        .create_key(
            "alive",
            PrincipalKind::Human,
            &names(&["trawl-admin"]),
            None,
        )
        .await
        .unwrap();
    let dead = store
        .create_key("dead", PrincipalKind::Human, &names(&["trawl-admin"]), None)
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
async fn assign_role_adds_and_rejects_duplicate(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;
    seed_roles(&store).await;

    let created = store
        .create_key("grantee", PrincipalKind::Service, &[], None)
        .await
        .unwrap();
    let prefix = KeyPrefix::parse(&created.info.prefix).expect("parse prefix");
    let role = RoleName::parse("trawl-admin").expect("parse role");

    keys::assign_role(&store, &prefix, &role)
        .await
        .expect("first assignment");

    // Same role again → RoleAlreadyAssigned, NOT silent no-op.
    let err = keys::assign_role(&store, &prefix, &role)
        .await
        .expect_err("duplicate assignment must error");
    assert!(
        matches!(
            err,
            AdminError::Auth(AuthError::RoleAlreadyAssigned { ref role, .. })
                if role == "trawl-admin"
        ),
        "expected RoleAlreadyAssigned, got {err:?}"
    );

    let second = RoleName::parse("coastwatch-siem_consumer").unwrap();
    keys::assign_role(&store, &prefix, &second)
        .await
        .expect("assignment of a different role");

    let info = store.get_key_by_prefix(&created.info.prefix).await.unwrap();
    assert_eq!(info.roles, ["coastwatch-siem_consumer", "trawl-admin"]);
}

#[sqlx::test(migrations = false)]
async fn assign_role_unknown_prefix_returns_key_not_found(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;
    seed_roles(&store).await;

    let prefix = KeyPrefix::parse("ghostpfx").unwrap();
    let role = RoleName::parse("trawl-admin").unwrap();
    let err = keys::assign_role(&store, &prefix, &role).await.unwrap_err();
    assert!(
        matches!(
            err,
            AdminError::Auth(AuthError::KeyNotFound { ref prefix }) if prefix == "ghostpfx"
        ),
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
    seed_roles(&store).await;

    let ninety_days = Duration::from_hours(90 * 24);
    let before = chrono::Utc::now();
    keys::create(
        &store,
        "exp",
        PrincipalKind::Service,
        &names(&["trawl-admin"]),
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
async fn unassign_role_removes_only_named_role(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;
    seed_roles(&store).await;

    let created = store
        .create_key(
            "multi",
            PrincipalKind::Service,
            &names(&["trawl-admin", "coastwatch-siem_consumer"]),
            None,
        )
        .await
        .unwrap();
    let prefix = KeyPrefix::parse(&created.info.prefix).expect("parse prefix");

    keys::unassign_role(
        &store,
        &prefix,
        &RoleName::parse("trawl-admin").unwrap(),
        true,
    )
    .await
    .expect("unassign role");

    let info = store.get_key_by_prefix(&created.info.prefix).await.unwrap();
    assert_eq!(
        info.roles,
        ["coastwatch-siem_consumer"],
        "other role must survive"
    );
}

#[sqlx::test(migrations = false)]
async fn unassign_role_not_held_returns_role_not_assigned(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;
    seed_roles(&store).await;

    let created = store
        .create_key(
            "bare",
            PrincipalKind::Service,
            &names(&["coastwatch-siem_consumer"]),
            None,
        )
        .await
        .unwrap();
    let prefix = KeyPrefix::parse(&created.info.prefix).expect("parse prefix");

    let err = keys::unassign_role(
        &store,
        &prefix,
        &RoleName::parse("trawl-admin").unwrap(),
        true,
    )
    .await
    .expect_err("missing assignment must error, not silently succeed");
    assert!(
        matches!(
            err,
            AdminError::Auth(AuthError::RoleNotAssigned { ref role, .. }) if role == "trawl-admin"
        ),
        "expected RoleNotAssigned, got {err:?}"
    );
}

#[sqlx::test(migrations = false)]
async fn unassign_role_unknown_prefix_returns_key_not_found(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;
    seed_roles(&store).await;

    let prefix = KeyPrefix::parse("ghostpfx").expect("parse prefix");
    let err = keys::unassign_role(
        &store,
        &prefix,
        &RoleName::parse("trawl-admin").unwrap(),
        true,
    )
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
    seed_roles(&store).await;

    let created = store
        .create_key("flip", PrincipalKind::Human, &names(&["trawl-admin"]), None)
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
