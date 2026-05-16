// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Embedded sqlx migrations.
//!
//! Why a hand-built `LazyLock<Migrator>` instead of `sqlx::migrate!()`?
//!
//! The `sqlx::migrate!()` macro lives in the `sqlx-macros` crate, which
//! depends on `sqlx-macros-core`, which declares `sqlx-sqlite` as an
//! optional dependency. Cargo's `links` uniqueness check evaluates optional
//! dependencies at resolve time, and `sqlx-sqlite`'s `libsqlite3-sys` link
//! conflicts with the `libsqlite3-sys` already pulled in by trawl-auth's
//! `rusqlite`. Going direct to `sqlx-postgres` + `sqlx-core` sidesteps the
//! collision (ADR-0029 / ADR-0030 context); the cost is that we hand-build
//! the migrator.
//!
//! Migration SQL is still embedded at compile time via `include_str!`, and
//! `MIGRATOR` is still a public, lazily-initialized `Migrator` that
//! `fleet-admin migrate` will consume via `MIGRATOR.run(&pool).await` in a
//! later slice.

use std::borrow::Cow;
use std::sync::LazyLock;

use sqlx_core::migrate::{Migration, MigrationType, Migrator};

/// Embedded schema migrations applied to the fleet database.
///
/// Run explicitly via `MIGRATOR.run(&pool).await` — never auto-applied by
/// `KeyStore::from_pool`. Two binaries (trawl-web, coastwatch-web) share
/// the same database; auto-migration would race on deploy.
pub static MIGRATOR: LazyLock<Migrator> = LazyLock::new(|| Migrator {
    migrations: Cow::Owned(vec![Migration::new(
        20_260_515_000_001,
        Cow::Borrowed("create api key keystore"),
        MigrationType::Simple,
        Cow::Borrowed(include_str!(
            "../migrations/0001_create_api_key_keystore.sql"
        )),
        false,
    )]),
    ignore_missing: false,
    locking: true,
    no_tx: false,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrator_exposes_one_migration() {
        // Smoke test that the Migrator builds and the SQL embed is non-empty.
        let migrations: Vec<_> = MIGRATOR.iter().collect();
        assert_eq!(migrations.len(), 1);
        assert_eq!(migrations[0].version, 20_260_515_000_001);
        assert!(migrations[0].sql.contains("CREATE TABLE api_keys"));
        assert!(
            migrations[0]
                .sql
                .contains("CREATE TABLE api_key_role_assignment")
        );
    }
}
