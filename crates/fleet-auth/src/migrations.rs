// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Embedded sqlx migrations.
//!
//! The migration filename carries the full `20260515000001` version prefix so
//! `sqlx::migrate!()` derives the SAME version the retired hand-built
//! `LazyLock<Migrator>` used — already-migrated fleet databases validate
//! cleanly (version + checksum match; the checksum covers only the SQL).
//!
//! History: until ADR-0004 slice 3 this was a hand-built `LazyLock<Migrator>`
//! because the workspace's rusqlite (`links = "sqlite3"`, via the now-deleted
//! trawl-auth crate) collided with sqlx-macros' optional sqlx-sqlite at
//! resolve time (issue #12). The collision died with the crate.

/// Embedded schema migrations applied to the fleet database.
///
/// Run explicitly via `MIGRATOR.run(&pool).await` — never auto-applied by
/// `KeyStore::from_pool`. Two binaries (trawl-web, coastwatch-web) share
/// the same database; auto-migration would race on deploy.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

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
