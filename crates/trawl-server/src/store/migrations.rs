// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Fresh-schema admission and forward migrations for Trawl app-state database.

use sqlx::{
    Connection as _, PgConnection, PgPool,
    migrate::{Migrate as _, MigrateError, Migrator},
};

/// First supported schema. Earlier deployment histories are not adopted.
pub const BASELINE_VERSION: i64 = 20_260_913_000_001;
const LEGACY_VERSIONS: &[i64] = &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17];

/// The embedded migrations. Boot applies them through [`migrate`];
/// [`validate_schema`] compares a ledger with them and changes nothing.
pub static MIGRATOR: Migrator = sqlx::migrate!();

/// Where a ledger that boot admits stands against the embedded migrations.
/// Every ledger boot refuses is a [`SchemaError`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ledger {
    /// No history and no application objects: boot creates the schema.
    Fresh,
    /// Every applied migration is embedded and intact, and `pending`
    /// embedded migrations are not applied yet: boot applies them.
    Behind {
        /// How many embedded migrations the ledger lacks.
        pending: usize,
    },
    /// Every embedded migration is applied, and nothing else is.
    Current,
}

/// Schema admission, validation, and execution failures remain distinguishable.
#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    /// A known pre-baseline installation must be retained, not converted.
    #[error(
        "Trawl app-state database has unsupported pre-1.0 migration {version}; provision a new dedicated database and retain the old database; do not delete or rewrite its migration history"
    )]
    LegacyHistory { version: i64 },
    /// Tables without a supported history must never be adopted implicitly.
    #[error(
        "Trawl app-state database is nonempty without a supported baseline; provision a new dedicated database and retain this database"
    )]
    UntrackedSchema,
    /// Preserve unknown versions, checksum mismatch, and dirty-current errors.
    #[error("Trawl app-state database migration error: {0}")]
    Migration(#[from] MigrateError),
    /// Database errors are logged locally and redacted at HTTP boundaries.
    #[error("Trawl app-state database schema check failed: {0}")]
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
        // Every ledger boot admits, fresh, behind or current, is brought
        // current the same way.
        let _: Ledger = check_history(&mut conn, migrator).await?;
        migrator.run_direct(None, &mut conn, false).await?;
        Ok(())
    }
    .await;
    let close = conn.close().await;
    result?;
    close?;
    Ok(())
}

/// Classify the app-state ledger as boot would admit it, in one read-only
/// snapshot, without taking the migrator's lock or changing anything
/// (`trawld --doctor`, ADR-0047). The snapshot is rolled back before this
/// returns; if the future is dropped part-way, the connection may still be
/// inside it, and the caller should close it.
///
/// # Errors
/// Every ledger boot refuses, as [`migrate`] reports it, and database
/// errors. A classification error wins over an error rolling the snapshot
/// back.
pub async fn validate_schema(conn: &mut PgConnection) -> Result<Ledger, SchemaError> {
    let mut snapshot = conn
        .begin_with("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .await?;
    let ledger = check_history(&mut snapshot, &MIGRATOR).await;
    let rollback = snapshot.rollback().await;
    let ledger = ledger?;
    rollback?;
    Ok(ledger)
}

/// Whether some session holds the `SQLx` migrator's advisory lock on the
/// database `conn` is connected to. Observed in `pg_locks`, never taken.
///
/// # Errors
/// Database errors.
pub async fn migrator_lock_held(conn: &mut PgConnection) -> Result<bool, sqlx::Error> {
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&mut *conn)
        .await?;
    super::advisory_lock_granted(conn, migrator_lock_key(&database)).await
}

/// The advisory lock key `SQLx` 0.9.0's Postgres migrator takes for
/// `database`: `sqlx-postgres-0.9.0/src/migrate.rs`, `generate_lock_id`,
/// is `0x3d32ad9e * (CRC-32/ISO-HDLC(current_database()) as i64)`, taken
/// with `pg_advisory_lock($1)` in `Migrate::lock` on the connection that
/// migrates. `fleet-admin` and trawld's boot both lock this way. The
/// product fits an `i64`: the CRC is below 2^32 and the factor below 2^30.
fn migrator_lock_key(database: &str) -> i64 {
    0x3d32_ad9e * i64::from(crc32_iso_hdlc(database.as_bytes()))
}

/// CRC-32/ISO-HDLC (the zlib and Ethernet CRC: reflected polynomial
/// `0xEDB88320`, initial value and final XOR all ones), which `SQLx`
/// computes through `crc::CRC_32_ISO_HDLC`. Bitwise: it runs once per
/// doctor run over a database name.
fn crc32_iso_hdlc(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// One applied row of `_sqlx_migrations`: version, success, checksum.
type AppliedRow = (i64, bool, Vec<u8>);

/// What the ledger holds, as [`check_history`] reads it.
enum Observed {
    /// No history rows. `untracked` says whether the database holds
    /// application relations, functions, or types all the same.
    Empty { untracked: bool },
    /// The history rows, by version.
    Applied(Vec<AppliedRow>),
}

/// Read the ledger, then classify it. Shared by boot ([`migrate`], under the
/// migrator's lock) and [`validate_schema`] (in a read-only snapshot).
async fn check_history(
    conn: &mut PgConnection,
    migrator: &Migrator,
) -> Result<Ledger, SchemaError> {
    let observed = observe_history(conn).await?;
    classify_history(&observed, migrator)
}

async fn observe_history(conn: &mut PgConnection) -> Result<Observed, sqlx::Error> {
    let has_ledger: bool = sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
        .fetch_one(&mut *conn)
        .await?;
    let history: Vec<AppliedRow> = if has_ledger {
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
        return Ok(Observed::Empty {
            untracked: nonempty,
        });
    }
    Ok(Observed::Applied(history))
}

/// Classify what [`observe_history`] read. Pure, so boot and the doctor
/// cannot disagree about a ledger: the refusals come first, in boot's
/// order, and only a ledger boot admits is `Fresh`, `Behind` or `Current`.
/// "Behind" means embedded versions are missing after every applied row
/// validated; an applied version the binary does not embed ("ahead") is
/// refused.
fn classify_history(observed: &Observed, migrator: &Migrator) -> Result<Ledger, SchemaError> {
    let history = match observed {
        Observed::Empty { untracked: true } => return Err(SchemaError::UntrackedSchema),
        Observed::Empty { untracked: false } => return Ok(Ledger::Fresh),
        Observed::Applied(history) => history,
    };
    for (version, success, checksum) in history {
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
    // Boot's runner applies up migrations only, so only those can pend.
    let pending = migrator
        .iter()
        .filter(|migration| !migration.migration_type.is_down_migration())
        .filter(|migration| {
            !history
                .iter()
                .any(|(version, ..)| *version == migration.version)
        })
        .count();
    Ok(if pending == 0 {
        Ledger::Current
    } else {
        Ledger::Behind { pending }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every embedded migration as a ledger row that validates.
    fn applied(migrator: &Migrator) -> Vec<AppliedRow> {
        migrator
            .iter()
            .map(|m| (m.version, true, m.checksum.to_vec()))
            .collect()
    }

    fn classify(rows: Vec<AppliedRow>) -> Result<Ledger, SchemaError> {
        classify_history(&Observed::Applied(rows), &MIGRATOR)
    }

    /// A ledger whose every row validates is behind by exactly the
    /// embedded migrations it lacks, and current when it lacks none. A gap
    /// counts the same as a missing tail.
    #[test]
    fn check_history_classifies_behind() {
        let all = applied(&MIGRATOR);
        assert!(
            all.len() >= 2,
            "the test needs a migration past the baseline"
        );
        assert_eq!(all[0].0, BASELINE_VERSION);
        assert_eq!(classify(all.clone()).unwrap(), Ledger::Current);
        assert_eq!(
            classify(all[..1].to_vec()).unwrap(),
            Ledger::Behind {
                pending: all.len() - 1
            }
        );
        let mut gap = all.clone();
        gap.remove(1);
        assert_eq!(classify(gap).unwrap(), Ledger::Behind { pending: 1 });
        assert_eq!(
            classify_history(&Observed::Empty { untracked: false }, &MIGRATOR).unwrap(),
            Ledger::Fresh
        );
    }

    /// The refusals come before "behind": a ledger that lacks migrations
    /// and also holds a row boot refuses is refused, as boot refuses it.
    #[test]
    fn check_history_refusals_win_over_behind() {
        let baseline = applied(&MIGRATOR)[..1].to_vec();
        let with = |row: AppliedRow| {
            let mut rows = baseline.clone();
            rows.push(row);
            rows.sort_by_key(|(version, ..)| *version);
            rows
        };
        assert!(matches!(
            classify(with((29_990_101_000_001, true, vec![0]))),
            Err(SchemaError::Migration(MigrateError::VersionMissing(
                29_990_101_000_001
            )))
        ));
        assert!(matches!(
            classify(with((17, true, vec![0]))),
            Err(SchemaError::LegacyHistory { version: 17 })
        ));
        let mut dirty = baseline.clone();
        dirty[0].1 = false;
        assert!(matches!(
            classify(dirty),
            Err(SchemaError::Migration(MigrateError::Dirty(
                BASELINE_VERSION
            )))
        ));
        let mut tampered = baseline.clone();
        tampered[0].2 = vec![0];
        assert!(matches!(
            classify(tampered),
            Err(SchemaError::Migration(MigrateError::VersionMismatch(
                BASELINE_VERSION
            )))
        ));
        // Applied rows past the baseline without the baseline itself.
        let past = applied(&MIGRATOR)[1..].to_vec();
        assert!(matches!(classify(past), Err(SchemaError::UntrackedSchema)));
        assert!(matches!(
            classify_history(&Observed::Empty { untracked: true }, &MIGRATOR),
            Err(SchemaError::UntrackedSchema)
        ));
    }

    /// The check value of CRC-32/ISO-HDLC, and the key it yields: the
    /// live proof that it is `SQLx`'s key is `doctor_detects_real_migrator_lock`,
    /// which takes the lock through `SQLx` itself.
    #[test]
    fn the_migrator_lock_key_is_sqlx_formula() {
        assert_eq!(crc32_iso_hdlc(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32_iso_hdlc(b""), 0);
        assert_eq!(
            migrator_lock_key("123456789"),
            0x3d32_ad9e * 0xCBF4_3926_i64
        );
        assert!(migrator_lock_key("trawl") > 0);
    }
}
