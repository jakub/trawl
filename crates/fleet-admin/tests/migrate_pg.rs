// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Integration coverage for `fleet-admin migrate` against a real Postgres.
//!
//! The `common::migrated_store` helper already runs `MIGRATOR.run` once, so
//! this file verifies the canonical entry point — `fleet_auth::MIGRATOR` —
//! is idempotent (a second run is a no-op against an already-migrated DB)
//! and that the expected tables are present.

mod common;

use fleet_auth::{MIGRATOR, PrincipalKind};
use sqlx::Row as _;

#[sqlx::test(migrations = false)]
async fn migrator_is_idempotent_on_already_migrated_db(pool: sqlx::PgPool) {
    let store = common::migrated_store(pool).await;

    // Helper already migrated once. Run again — must be a no-op.
    MIGRATOR
        .run(store.pool())
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
        "legacy api_key_role_assignment must be dropped by the roles-as-data migration; tables: {tables:?}"
    );
}
