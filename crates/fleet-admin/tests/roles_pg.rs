// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Integration coverage for `fleet-admin roles` against a real Postgres
//! (ADR-0006 slice 1, issue #44 AC7).
//!
//! Command functions are exercised where they add behaviour over the
//! store (confirmation flow, warn-only registry surface); pure store
//! round-trips call `KeyStore` directly, mirroring `keys_pg.rs`.

mod common;

use fleet_admin::commands::keys::RoleName;
use fleet_admin::commands::roles;
use fleet_admin::error::AdminError;
use fleet_auth::{AuthError, PrincipalKind, RolePermission};

fn rp(app: &str, permission: &str) -> RolePermission {
    RolePermission {
        app: app.into(),
        permission: permission.into(),
    }
}

fn role_name(s: &str) -> RoleName {
    RoleName::parse(s).expect("valid role name")
}

#[sqlx::test(migrations = false)]
async fn create_show_roundtrip(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;

    roles::create(
        &store,
        &role_name("tier1"),
        &[rp("trawl", "query"), rp("trawl", "schema_read")],
        Some(120),
    )
    .await
    .expect("create role");

    let role = store.get_role("tier1").await.expect("get role");
    assert_eq!(role.name, "tier1");
    assert_eq!(role.rate_rpm, Some(120));
    assert_eq!(
        role.permissions,
        vec![rp("trawl", "query"), rp("trawl", "schema_read")]
    );

    roles::show(&store, &role_name("tier1"))
        .await
        .expect("show role");
}

#[sqlx::test(migrations = false)]
async fn create_duplicate_is_role_exists(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;

    roles::create(&store, &role_name("dup"), &[], None)
        .await
        .expect("first create");
    let err = roles::create(&store, &role_name("dup"), &[], None)
        .await
        .expect_err("duplicate must error");
    assert!(
        matches!(err, AdminError::Auth(AuthError::RoleExists { ref name }) if name == "dup"),
        "expected RoleExists, got {err:?}"
    );
}

#[sqlx::test(migrations = false)]
async fn unknown_permission_warns_but_persists(pool: sqlx::PgPool) {
    // AC7: the registry is warn-only. `qyery` is not in app_permissions —
    // the command must still succeed (exit 0 at the binary boundary is
    // "returns Ok here") and the row must be persisted. The stderr warning
    // text itself is exercised by the binary; here we pin the contract that
    // matters: no error, row persisted, registry consulted.
    let store = common::migrated_store(pool).await;

    roles::create(&store, &role_name("typo"), &[rp("trawl", "qyery")], None)
        .await
        .expect("unknown permission must not fail the command");

    let role = store.get_role("typo").await.unwrap();
    assert_eq!(
        role.permissions,
        vec![rp("trawl", "qyery")],
        "row persisted"
    );
    assert!(!store.is_known_permission("trawl", "qyery").await.unwrap());
    assert!(store.is_known_permission("trawl", "query").await.unwrap());
}

#[sqlx::test(migrations = false)]
async fn add_and_remove_perm_roundtrip(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;

    roles::create(&store, &role_name("mut"), &[rp("trawl", "query")], None)
        .await
        .unwrap();

    roles::add_perm(&store, &role_name("mut"), &[rp("trawl", "export")])
        .await
        .expect("add perm");
    let role = store.get_role("mut").await.unwrap();
    assert_eq!(
        role.permissions,
        vec![rp("trawl", "export"), rp("trawl", "query")]
    );

    roles::remove_perm(&store, &role_name("mut"), &[rp("trawl", "export")])
        .await
        .expect("remove perm");
    let role = store.get_role("mut").await.unwrap();
    assert_eq!(role.permissions, vec![rp("trawl", "query")]);

    // Removing a pair that isn't on the role is a warning, not an error.
    roles::remove_perm(&store, &role_name("mut"), &[rp("trawl", "export")])
        .await
        .expect("removing an absent pair warns but succeeds");
}

#[sqlx::test(migrations = false)]
async fn add_perm_unknown_role_is_role_not_found(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;

    let err = roles::add_perm(&store, &role_name("ghost"), &[rp("trawl", "query")])
        .await
        .expect_err("unknown role must error");
    assert!(
        matches!(err, AdminError::Auth(AuthError::RoleNotFound { ref name }) if name == "ghost"),
        "expected RoleNotFound, got {err:?}"
    );
}

#[sqlx::test(migrations = false)]
async fn delete_refuses_in_use_role_without_force(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;

    roles::create(&store, &role_name("held"), &[rp("trawl", "query")], None)
        .await
        .unwrap();
    let created = store
        .create_key("holder", PrincipalKind::Service, &["held".to_owned()], None)
        .await
        .unwrap();

    // --yes skips the prompt; without --force the store must refuse and
    // name the affected-key count.
    let err = roles::delete(&store, &role_name("held"), false, true)
        .await
        .expect_err("in-use role must be refused without force");
    assert!(
        matches!(
            err,
            AdminError::Auth(AuthError::RoleInUse { ref name, key_count })
                if name == "held" && key_count == 1
        ),
        "expected RoleInUse with key_count=1, got {err:?}"
    );

    // Forced: role gone, key stripped.
    roles::delete(&store, &role_name("held"), true, true)
        .await
        .expect("forced delete");
    assert!(matches!(
        store.get_role("held").await.unwrap_err(),
        AuthError::RoleNotFound { .. }
    ));
    let info = store.get_key_by_prefix(&created.info.prefix).await.unwrap();
    assert!(info.roles.is_empty(), "forced delete strips the key");
}

#[sqlx::test(migrations = false)]
async fn delete_unheld_role_succeeds(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;

    roles::create(&store, &role_name("loose"), &[], None)
        .await
        .unwrap();
    roles::delete(&store, &role_name("loose"), false, true)
        .await
        .expect("delete unheld role");
    assert!(matches!(
        store.get_role("loose").await.unwrap_err(),
        AuthError::RoleNotFound { .. }
    ));
}

#[sqlx::test(migrations = false)]
async fn schema_write_is_grantable_through_a_role(pool: sqlx::PgPool) {
    // ADR-0011 slice B (issue #53): the migration REGISTERS
    // `trawl:schema_write` and grants it to nobody. The operator's whole
    // path to a schema-admin is therefore one `fleet-admin roles create`
    // — no deploy, and no `server_manage` riding along.
    let store = common::migrated_store(pool).await;

    // Registered vocabulary: naming it in a role mutation warns about
    // nothing (the registry is the warn-only surface exercised above).
    assert!(
        store
            .is_known_permission("trawl", "schema_write")
            .await
            .unwrap(),
        "slice B must register trawl:schema_write in app_permissions"
    );

    // ...and no converted tier holds it before the operator acts.
    for role in store.list_roles().await.unwrap() {
        assert!(
            !role.permissions.contains(&rp("trawl", "schema_write")),
            "role {} gained schema_write from a migration: {:?}",
            role.name,
            role.permissions
        );
    }

    roles::create(
        &store,
        &role_name("trawl-schema-admin"),
        &[rp("trawl", "schema_read"), rp("trawl", "schema_write")],
        None,
    )
    .await
    .expect("create the schema-admin role");

    let created = store
        .create_key(
            "schema-admin",
            PrincipalKind::Service,
            &["trawl-schema-admin".to_owned()],
            None,
        )
        .await
        .unwrap();

    // The grant resolves through the same verify path the server gates on.
    let verified = store.verify_key(&created.plaintext_token).await.unwrap();
    assert_eq!(verified.roles(), ["trawl-schema-admin"]);
    assert!(verified.has_app_permission("trawl", "schema_write"));
    assert!(verified.has_app_permission("trawl", "schema_read"));
    assert!(
        !verified.has_app_permission("trawl", "server_manage"),
        "a schema-admin must not need server_manage"
    );

    // And it is revocable the same way it was granted.
    roles::remove_perm(
        &store,
        &role_name("trawl-schema-admin"),
        &[rp("trawl", "schema_write")],
    )
    .await
    .expect("remove schema_write");
    let verified = store.verify_key(&created.plaintext_token).await.unwrap();
    assert!(!verified.has_app_permission("trawl", "schema_write"));
}

#[sqlx::test(migrations = false)]
async fn assign_unassign_roundtrip_through_role_commands(pool: sqlx::PgPool) {
    // AC7 round-trip: roles create → keys create --role → assign/unassign
    // visible through the verify path.
    let store = common::migrated_store(pool).await;

    roles::create(&store, &role_name("t1"), &[rp("trawl", "query")], None)
        .await
        .unwrap();
    roles::create(&store, &role_name("t2"), &[rp("trawl", "export")], None)
        .await
        .unwrap();

    let created = store
        .create_key("worker", PrincipalKind::Service, &["t1".to_owned()], None)
        .await
        .unwrap();

    store.assign_role(&created.info.prefix, "t2").await.unwrap();
    let verified = store.verify_key(&created.plaintext_token).await.unwrap();
    assert_eq!(verified.roles(), ["t1", "t2"]);
    assert!(verified.has_app_permission("trawl", "export"));

    store
        .unassign_role(&created.info.prefix, "t1")
        .await
        .unwrap();
    let verified = store.verify_key(&created.plaintext_token).await.unwrap();
    assert_eq!(verified.roles(), ["t2"]);
    assert!(!verified.has_app_permission("trawl", "query"));
}

#[sqlx::test(migrations = false)]
async fn list_handles_empty_and_populated(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;

    roles::list(&store).await.expect("empty list");
    roles::create(&store, &role_name("a"), &[], None)
        .await
        .unwrap();
    roles::create(&store, &role_name("b"), &[rp("trawl", "query")], Some(60))
        .await
        .unwrap();
    roles::list(&store).await.expect("populated list");

    let all = store.list_roles().await.unwrap();
    let names: Vec<&str> = all.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["a", "b"]);
}
