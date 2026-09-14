// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Integration coverage for `fleet-admin migrate` against a real Postgres.
//!
//! The `common::migrated_store` helper already runs `migrate` once, so
//! this file verifies the canonical entry point — `fleet_auth::migrate` —
//! is idempotent (a second run is a no-op against an already-migrated DB)
//! and that the expected tables are present.

mod common;

use fleet_auth::{PrincipalKind, migrate};
use sqlx::Row as _;

#[sqlx::test(migrations = false)]
async fn migrator_is_idempotent_on_already_migrated_db(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;

    // Helper already migrated once. Run again — must be a no-op.
    migrate(store.pool())
        .await
        .expect("second run is a no-op against migrated schema");

    let created = store
        .create_key("post-migrate", PrincipalKind::Human, &[], None)
        .await
        .expect("create_key after second migrate");
    assert!(created.plaintext_token.starts_with("flt_"));
}

#[sqlx::test(migrations = false)]
async fn migrator_creates_expected_tables(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;

    let tables: Vec<String> = sqlx::query(
        "SELECT table_name FROM information_schema.tables
         WHERE table_schema = 'public'
         ORDER BY table_name",
    )
    .fetch_all(store.pool())
    .await
    .expect("query information_schema")
    .iter()
    .map(|r| r.try_get::<String, _>("table_name").unwrap())
    .collect();

    for expected in [
        "api_keys",
        "roles",
        "role_permissions",
        "key_roles",
        "app_permissions",
    ] {
        assert!(
            tables.iter().any(|t| t == expected),
            "missing {expected}; tables: {tables:?}"
        );
    }
    assert!(
        !tables.iter().any(|t| t == "api_key_role_assignment"),
        "initial schema must not create api_key_role_assignment; tables: {tables:?}"
    );
}

/// The CLI must refuse old history before either administrative write path.
#[sqlx::test(migrations = false)]
async fn cli_refuses_old_history_before_role_or_key_writes(pool: sqlx::PgPool) {
    let old = sqlx::migrate::Migrator::new(std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../scripts/schema-baseline/fixtures/fleet"
    )))
    .await
    .unwrap();
    old.run(&pool).await.unwrap();
    let history: Vec<String> =
        sqlx::query_scalar("SELECT to_jsonb(m)::text FROM _sqlx_migrations m ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    let url = common::isolated_database_url(&pool);
    for args in [
        vec!["roles", "create", "--name", "must-not-exist"],
        vec![
            "keys",
            "create",
            "--name",
            "must-not-exist",
            "--kind",
            "service",
        ],
        vec!["migrate"],
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_fleet-admin"))
            .args(&args)
            .env("DATABASE_URL", &url)
            .output()
            .unwrap();
        // Do not print stdout: a broken key-create guard could emit a token.
        assert!(
            !output.status.success(),
            "command {args:?} accepted an old schema"
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported pre-1.0 migration"));
        assert!(output.stdout.is_empty());
    }
    let counts: (i64, i64) =
        sqlx::query_as("SELECT (SELECT count(*) FROM roles),(SELECT count(*) FROM api_keys)")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(counts, (0, 0));
    let after: Vec<String> =
        sqlx::query_scalar("SELECT to_jsonb(m)::text FROM _sqlx_migrations m ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(after, history);
}

#[sqlx::test(migrations = false)]
async fn cli_allows_role_and_key_writes_on_current_history(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;
    let url = common::isolated_database_url(store.pool());
    for args in [
        vec!["roles", "create", "--name", "current-role"],
        vec![
            "keys",
            "create",
            "--name",
            "current-key",
            "--kind",
            "service",
        ],
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_fleet-admin"))
            .args(&args)
            .env("DATABASE_URL", &url)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "command {args:?} rejected current history"
        );
    }
    let counts: (i64, i64) =
        sqlx::query_as("SELECT (SELECT count(*) FROM roles),(SELECT count(*) FROM api_keys)")
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(counts, (1, 1));
}
