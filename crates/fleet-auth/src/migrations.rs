// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Fresh-schema admission and forward migrations for Fleet database.

use sqlx::{
    Connection as _, PgConnection, PgPool,
    migrate::{Migrate as _, MigrateError, Migrator},
};

/// First supported schema. Earlier deployment histories are not adopted.
pub const BASELINE_VERSION: i64 = 20_260_913_000_001;
const LEGACY_VERSIONS: &[i64] = &[20_260_515_000_001, 20_260_726_000_001, 20_260_812_000_001];

/// Embedded forward migrations, also available to `SQLx` test infrastructure.
/// Operational callers use `migrate`, which enforces fresh-schema admission.
pub static MIGRATOR: Migrator = sqlx::migrate!();

/// Schema admission, validation, and execution failures remain distinguishable.
#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    /// A known pre-baseline installation must be retained, not converted.
    #[error(
        "Fleet database has unsupported pre-1.0 migration {version}; provision a new dedicated database and retain the old database; do not delete or rewrite its migration history"
    )]
    LegacyHistory { version: i64 },
    /// Tables without a supported history must never be adopted implicitly.
    #[error(
        "Fleet database is nonempty without a supported baseline; provision a new dedicated database and retain this database"
    )]
    UntrackedSchema,
    /// Runtime validation never initializes an empty database.
    #[error(
        "Fleet database has no initial schema; apply its migrations to a fresh dedicated database before starting the application"
    )]
    Uninitialized,
    /// Known forward migration has not yet been applied.
    #[error(
        "Fleet database needs migration {version}; apply its migrations before starting the application"
    )]
    PendingMigration { version: i64 },
    /// Preserve unknown versions, checksum mismatch, and dirty-current errors.
    #[error("Fleet database migration error: {0}")]
    Migration(#[from] MigrateError),
    /// Database errors are logged locally and redacted at HTTP boundaries.
    #[error("Fleet database schema check failed: {0}")]
    Database(#[from] sqlx::Error),
}

/// Apply pending migrations under the same `SQLx` lock as the admission check.
/// The detached connection cannot return a session lock to a pool, including
/// when this future is cancelled. Closing it also releases locks after errors.
pub async fn migrate(pool: &PgPool) -> Result<(), SchemaError> {
    let mut migrator = sqlx::migrate!();
    migrator.set_locking(false);
    migrate_with(pool, &migrator).await
}

async fn migrate_with(pool: &PgPool, migrator: &Migrator) -> Result<(), SchemaError> {
    let mut conn = pool.acquire().await?.detach();
    let result: Result<(), SchemaError> = async {
        conn.lock().await?;
        check_history(&mut conn, migrator, false).await?;
        migrator.run_direct(None, &mut conn, false).await?;
        Ok(())
    }
    .await;
    let close = conn.close().await;
    result?;
    close?;
    Ok(())
}

/// Validate the complete known history without applying migrations or DDL.
/// One read-only snapshot prevents ledger reads spanning a concurrent commit.
pub async fn validate_schema(pool: &PgPool) -> Result<(), SchemaError> {
    let mut conn = pool.acquire().await?.detach();
    let result: Result<(), SchemaError> = async {
        sqlx::query("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut conn)
            .await?;
        check_history(&mut conn, &MIGRATOR, true).await
    }
    .await;
    // Closing rolls back the read-only transaction and never recycles it.
    let close = conn.close().await;
    result?;
    close?;
    Ok(())
}

async fn check_history(
    conn: &mut PgConnection,
    migrator: &Migrator,
    require_current: bool,
) -> Result<(), SchemaError> {
    let has_ledger: bool = sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
        .fetch_one(&mut *conn)
        .await?;
    let history: Vec<(i64, bool, Vec<u8>)> = if has_ledger {
        sqlx::query_as("SELECT version, success, checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&mut *conn)
            .await?
    } else {
        Vec::new()
    };
    if history.is_empty() {
        // A dedicated fresh DB may have its empty SQLx ledger, but no other
        // application relations, functions, or types. Check every user schema
        // so a different search_path cannot hide existing application state.
        let nonempty: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                WHERE n.nspname !~ '^pg_' AND n.nspname <> 'information_schema'
                  AND c.oid <> COALESCE(to_regclass('_sqlx_migrations'), 0)
                  AND NOT EXISTS (SELECT 1 FROM pg_index i WHERE i.indexrelid = c.oid
                                  AND i.indrelid = to_regclass('_sqlx_migrations'))
                UNION ALL
                SELECT 1 FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
                WHERE n.nspname !~ '^pg_' AND n.nspname <> 'information_schema'
                UNION ALL
                SELECT 1 FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace
                WHERE n.nspname !~ '^pg_' AND n.nspname <> 'information_schema'
                  AND (to_regclass('_sqlx_migrations') IS NULL
                       OR t.typrelid <> to_regclass('_sqlx_migrations'))
                  AND t.typelem = 0
            )",
        )
        .fetch_one(&mut *conn)
        .await?;
        if nonempty {
            return Err(SchemaError::UntrackedSchema);
        }
        return if require_current {
            Err(SchemaError::Uninitialized)
        } else {
            Ok(())
        };
    }
    for (version, success, checksum) in &history {
        if LEGACY_VERSIONS.contains(version) {
            return Err(SchemaError::LegacyHistory { version: *version });
        }
        let Some(migration) = migrator.iter().find(|m| m.version == *version) else {
            return Err(MigrateError::VersionMissing(*version).into());
        };
        if !success {
            return Err(MigrateError::Dirty(*version).into());
        }
        if migration.checksum.as_ref() != checksum.as_slice() {
            return Err(MigrateError::VersionMismatch(*version).into());
        }
    }
    if !history
        .iter()
        .any(|(version, _, _)| *version == BASELINE_VERSION)
    {
        return Err(SchemaError::UntrackedSchema);
    }
    if require_current {
        for migration in migrator.iter() {
            if !history
                .iter()
                .any(|(version, _, _)| *version == migration.version)
            {
                return Err(SchemaError::PendingMigration {
                    version: migration.version,
                });
            }
        }
    }
    Ok(())
}
