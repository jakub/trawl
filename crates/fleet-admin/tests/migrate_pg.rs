// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Integration coverage for `fleet-admin migrate` against a real Postgres.
//!
//! The `PgFixture` already runs `MIGRATOR.run` once during setup, so this
//! file verifies the canonical entry point — `fleet_auth::MIGRATOR` — is
//! idempotent (a second run is a no-op against an already-migrated DB)
//! and that the expected tables are present.

#[macro_use]
mod common;

use fleet_auth::{KeyStore, MIGRATOR, PrincipalKind};
use sqlx_core::row::Row as _;

pg_test!(
    migrator_is_idempotent_on_already_migrated_db,
    |store: KeyStore| async move {
        // Fixture already migrated once. Run again — must be a no-op.
        MIGRATOR
            .run(store.pool())
            .await
            .expect("second run is a no-op against migrated schema");

        // And the keystore still works end-to-end.
        let created = store
            .create_key("post-migrate", PrincipalKind::Human, &[], None)
            .await
            .expect("create_key after second migrate");
        assert!(created.plaintext_token.starts_with("flt_"));
    }
);

pg_test!(
    migrator_creates_expected_tables,
    |store: KeyStore| async move {
        let tables: Vec<String> = sqlx_core::query::query(
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

        assert!(
            tables.iter().any(|t| t == "api_keys"),
            "missing api_keys; tables: {tables:?}"
        );
        assert!(
            tables.iter().any(|t| t == "api_key_role_assignment"),
            "missing api_key_role_assignment; tables: {tables:?}"
        );
    }
);
