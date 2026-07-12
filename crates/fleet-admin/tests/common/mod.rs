// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared helper for fleet-admin's `#[sqlx::test]` integration tests.
//!
//! fleet-admin has no migrations directory of its own — the fleet schema is
//! fleet-auth's — so its tests run `#[sqlx::test(migrations = false)]` and
//! apply `fleet_auth::MIGRATOR` explicitly (single source of truth for the
//! schema; the migrate tests exercise exactly this entry point).

#![allow(dead_code)] // each test binary uses a subset of these items

use fleet_auth::KeyStore;
use sqlx::PgPool;

/// Apply the fleet-auth migrations to the per-test database and wrap it in
/// a [`KeyStore`].
pub async fn migrated_store(pool: PgPool) -> KeyStore {
    fleet_auth::MIGRATOR
        .run(&pool)
        .await
        .expect("apply fleet-auth migrations to per-test database");
    KeyStore::from_pool(pool)
}
