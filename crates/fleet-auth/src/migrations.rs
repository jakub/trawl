// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Embedded sqlx migrations.
//!
//! sqlx identifies an applied migration by the version number in its filename
//! and stores a checksum over its SQL. Renumber a shipped file or edit its SQL
//! and every already-migrated fleet database fails validation on the next run
//! (`VersionMissing` / `VersionMismatch`); add a new file instead.

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
    fn migrator_exposes_every_migration() {
        let migrations: Vec<_> = MIGRATOR.iter().collect();
        assert_eq!(migrations.len(), 3);
        assert_eq!(migrations[0].version, 20_260_515_000_001);
        assert!(migrations[0].sql.as_str().contains("CREATE TABLE api_keys"));
        assert_eq!(migrations[1].version, 20_260_726_000_001);
        assert!(migrations[1].sql.as_str().contains("CREATE TABLE roles"));
        assert!(
            migrations[1]
                .sql
                .as_str()
                .contains("DROP TABLE api_key_role_assignment")
        );
        // schema_write is registered in the vocabulary, never granted.
        assert_eq!(migrations[2].version, 20_260_812_000_001);
        let registry = migrations[2].sql.as_str();
        assert!(registry.contains("'schema_write'"));
        assert!(registry.contains("INSERT INTO app_permissions"));
        assert!(
            !registry.contains("role_permissions") && !registry.contains("key_roles"),
            "the registry migration must grant nothing"
        );
    }
}
