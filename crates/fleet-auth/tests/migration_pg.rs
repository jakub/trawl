// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Conversion-equivalence tests for the roles-as-data migration
//! (ADR-0006 slice 1, issue #44 AC1).
//!
//! Applies the base keystore migration by hand, seeds legacy-shape keys
//! with raw SQL (the pre-migration `api_key_role_assignment` grants),
//! applies the roles-as-data migration, and asserts every key's resolved
//! per-app permission set is exactly what the old compile-time tables
//! granted (trawl's minus `key_manage`, coastwatch's `permissions_for`).

#![cfg(feature = "keystore")]

use std::collections::BTreeSet;

use fleet_auth::{KeyStore, token};
use sqlx::PgPool;

const BASE_MIGRATION: &str =
    include_str!("../migrations/20260515000001_create_api_key_keystore.sql");
const ROLES_MIGRATION: &str = include_str!("../migrations/20260726000001_roles_as_data.sql");

/// The legacy trawl role → permission tables (policy.rs), minus the dead
/// `key_manage` the migration intentionally drops.
fn legacy_trawl_permissions(role: &str) -> Vec<&'static str> {
    match role {
        "admin" => vec![
            "query",
            "schema_read",
            "validate",
            "saved_query",
            "export",
            "stream",
            "query_cancel",
            "server_manage",
        ],
        "analyst" => vec![
            "query",
            "schema_read",
            "validate",
            "saved_query",
            "export",
            "stream",
            "query_cancel",
        ],
        "reader" => vec!["query", "schema_read", "query_cancel"],
        "ingest" => vec!["ingest"],
        other => panic!("unknown legacy trawl role {other}"),
    }
}

/// The legacy coastwatch `permissions_for` table, `snake_case` of the
/// variant names — the cross-repo wire contract this migration freezes.
fn legacy_coastwatch_permissions(role: &str) -> Vec<&'static str> {
    match role {
        "analyst" => vec![
            "stories_read",
            "analyst_decisions_write",
            "editions_review",
            "editions_revise",
            "editions_correct",
            "ioc_exports_read",
            "pipeline_read",
            "document_upload",
        ],
        "operator" => vec![
            "system_config_manage",
            "api_keys_manage",
            "editions_revise",
            "pipeline_read",
            "document_upload",
        ],
        "siem_consumer" => vec!["ioc_exports_read"],
        other => panic!("unknown legacy coastwatch role {other}"),
    }
}

/// Insert a legacy-shape key + grants with raw SQL and return
/// `(key_id, plaintext_token)`. The token is real (generated + hashed via
/// the token module) so post-migration `verify_key` can be exercised.
async fn seed_legacy_key(pool: &PgPool, name: &str, grants: &[(&str, &str)]) -> (i64, String) {
    let generated = token::generate_token();
    let hash = token::hash_token(&generated.plaintext).expect("hash");

    let key_id: i64 = sqlx::query_scalar(
        "INSERT INTO api_keys (prefix, name, hash, kind) VALUES ($1, $2, $3, 'service')
         RETURNING id",
    )
    .bind(&generated.prefix)
    .bind(name)
    .bind(&hash)
    .fetch_one(pool)
    .await
    .expect("insert legacy key");

    for (app, role) in grants {
        sqlx::query("INSERT INTO api_key_role_assignment (key_id, app, role) VALUES ($1, $2, $3)")
            .bind(key_id)
            .bind(app)
            .bind(role)
            .execute(pool)
            .await
            .expect("insert legacy grant");
    }

    (key_id, generated.plaintext.to_string())
}

/// Resolved per-app permission set for a key, straight from the new tables.
async fn resolved_permissions(pool: &PgPool, key_id: i64, app: &str) -> BTreeSet<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT rp.permission
         FROM key_roles kr
         JOIN role_permissions rp ON rp.role_id = kr.role_id
         WHERE kr.key_id = $1 AND rp.app = $2",
    )
    .bind(key_id)
    .bind(app)
    .fetch_all(pool)
    .await
    .expect("resolve permissions")
    .into_iter()
    .collect()
}

async fn role_names(pool: &PgPool, key_id: i64) -> Vec<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT r.name FROM key_roles kr JOIN roles r ON r.id = kr.role_id
         WHERE kr.key_id = $1 ORDER BY r.name",
    )
    .bind(key_id)
    .fetch_all(pool)
    .await
    .expect("role names")
}

fn as_set(perms: &[&str]) -> BTreeSet<String> {
    perms.iter().map(|s| (*s).to_owned()).collect()
}

#[sqlx::test(migrations = false)]
async fn ac1_conversion_preserves_every_legacy_grant(pool: PgPool) {
    sqlx::raw_sql(BASE_MIGRATION)
        .execute(&pool)
        .await
        .expect("base migration");

    // Legacy fixture matrix: the four trawl roles, the three coastwatch
    // roles, and one dual-app key.
    let trawl_roles = ["admin", "analyst", "reader", "ingest"];
    let coastwatch_roles = ["analyst", "operator", "siem_consumer"];

    let mut trawl_keys = Vec::new();
    for role in trawl_roles {
        let seeded = seed_legacy_key(&pool, &format!("t-{role}"), &[("trawl", role)]).await;
        trawl_keys.push((role, seeded));
    }
    let mut coastwatch_keys = Vec::new();
    for role in coastwatch_roles {
        let seeded = seed_legacy_key(&pool, &format!("cw-{role}"), &[("coastwatch", role)]).await;
        coastwatch_keys.push((role, seeded));
    }
    let (dual_id, dual_token) = seed_legacy_key(
        &pool,
        "dual",
        &[("trawl", "analyst"), ("coastwatch", "siem_consumer")],
    )
    .await;

    sqlx::raw_sql(ROLES_MIGRATION)
        .execute(&pool)
        .await
        .expect("roles-as-data migration");

    // Every trawl key resolves exactly the old compile-time set, minus
    // key_manage, and links to the converted `trawl-<role>` role.
    for (role, (key_id, _)) in &trawl_keys {
        let resolved = resolved_permissions(&pool, *key_id, "trawl").await;
        assert_eq!(
            resolved,
            as_set(&legacy_trawl_permissions(role)),
            "trawl {role} permission set drifted in conversion"
        );
        assert!(
            !resolved.contains("key_manage"),
            "dead key_manage must not be carried over"
        );
        assert_eq!(
            role_names(&pool, *key_id).await,
            vec![format!("trawl-{role}")]
        );
    }

    // Every coastwatch key resolves the frozen `permissions_for` set.
    for (role, (key_id, _)) in &coastwatch_keys {
        assert_eq!(
            resolved_permissions(&pool, *key_id, "coastwatch").await,
            as_set(&legacy_coastwatch_permissions(role)),
            "coastwatch {role} permission set drifted in conversion"
        );
        assert_eq!(
            role_names(&pool, *key_id).await,
            vec![format!("coastwatch-{role}")]
        );
    }

    // The dual-app key holds both converted roles and both permission sets.
    assert_eq!(
        role_names(&pool, dual_id).await,
        vec![
            "coastwatch-siem_consumer".to_owned(),
            "trawl-analyst".to_owned()
        ]
    );
    assert_eq!(
        resolved_permissions(&pool, dual_id, "trawl").await,
        as_set(&legacy_trawl_permissions("analyst"))
    );
    assert_eq!(
        resolved_permissions(&pool, dual_id, "coastwatch").await,
        as_set(&legacy_coastwatch_permissions("siem_consumer"))
    );

    // A pre-migration token still authorizes identically through the new
    // resolution path.
    let store = KeyStore::from_pool(pool.clone());
    let verified = store.verify_key(&dual_token).await.expect("verify");
    assert_eq!(
        verified.roles(),
        ["coastwatch-siem_consumer", "trawl-analyst"]
    );
    assert!(verified.has_app_permission("trawl", "query"));
    assert!(verified.has_app_permission("coastwatch", "ioc_exports_read"));
    assert!(!verified.has_app_permission("trawl", "key_manage"));

    // The legacy table is gone.
    let legacy_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pg_tables WHERE tablename = 'api_key_role_assignment')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!legacy_exists, "api_key_role_assignment must be dropped");

    // trawl's 9 live permission strings are registered; key_manage is not.
    let registry: BTreeSet<String> = sqlx::query_scalar::<_, String>(
        "SELECT permission FROM app_permissions WHERE app = 'trawl'",
    )
    .fetch_all(&pool)
    .await
    .unwrap()
    .into_iter()
    .collect();
    assert_eq!(
        registry,
        as_set(&[
            "query",
            "schema_read",
            "validate",
            "saved_query",
            "export",
            "stream",
            "query_cancel",
            "server_manage",
            "ingest",
        ])
    );
}

/// Legacy grants v1 allowed but v2 role names forbid must fail the
/// pre-flight with the offending pairs named — not with an opaque
/// `roles_name_format` CHECK violation.
#[sqlx::test(migrations = false)]
async fn conversion_names_legacy_grants_it_cannot_convert(pool: PgPool) {
    sqlx::raw_sql(BASE_MIGRATION)
        .execute(&pool)
        .await
        .expect("base migration");

    // Uppercase (v1 only banned control chars) and a lowercase role that
    // only overflows 64 bytes once `trawl-` is prepended.
    let long_role = "r".repeat(60);
    seed_legacy_key(&pool, "legacy-upper", &[("trawl", "Admin")]).await;
    seed_legacy_key(&pool, "legacy-long", &[("trawl", &long_role)]).await;

    let err = sqlx::raw_sql(ROLES_MIGRATION)
        .execute(&pool)
        .await
        .expect_err("migration must refuse unconvertible legacy grants");
    let msg = err.to_string();
    assert!(msg.contains("Admin"), "offending role not named: {msg}");
    assert!(msg.contains(&long_role), "overlong role not named: {msg}");

    // Aborting the migration must leave the legacy table intact so the
    // operator can rename and re-run.
    let legacy_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pg_tables WHERE tablename = 'api_key_role_assignment')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(legacy_exists, "failed migration must roll back cleanly");
}

#[sqlx::test(migrations = false)]
async fn conversion_dedupes_shared_roles_across_keys(pool: PgPool) {
    sqlx::raw_sql(BASE_MIGRATION)
        .execute(&pool)
        .await
        .expect("base migration");

    // Two keys with the same legacy (app, role) must converge on ONE role.
    let (a, _) = seed_legacy_key(&pool, "a", &[("trawl", "reader")]).await;
    let (b, _) = seed_legacy_key(&pool, "b", &[("trawl", "reader")]).await;

    sqlx::raw_sql(ROLES_MIGRATION)
        .execute(&pool)
        .await
        .expect("roles-as-data migration");

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM roles WHERE name = 'trawl-reader'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "one role per distinct legacy (app, role) pair");
    assert_eq!(role_names(&pool, a).await, role_names(&pool, b).await);
}
