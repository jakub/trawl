//! Real PostgreSQL checks shared by the two independent schema owners.
//! Every database here belongs to `sqlx::test` on an explicitly owned instance.
use super::{BASELINE, NEW_DIR, OLD_DIR, REFERENCE_AMENDMENT, SchemaError, migrate, seed_old};
use sqlx::{
    Connection as _, Executor as _, PgConnection, PgPool,
    migrate::{Migrate as _, MigrateError, Migrator},
};
use std::time::Duration;

async fn catalog(conn: &mut PgConnection) -> Vec<(String, String)> {
    let mut records: Vec<(String, String)> = sqlx::query_as(
        r"SELECT kind, body::text FROM (
          SELECT 'column' AS kind, jsonb_build_array(c.relname, a.attnum, a.attname,
            format_type(a.atttypid,a.atttypmod), a.attnotnull, a.attidentity,
            a.attgenerated, pg_get_expr(d.adbin,d.adrelid), col.collname) AS body
          FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid
          JOIN pg_namespace n ON n.oid=c.relnamespace
          LEFT JOIN pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum
          LEFT JOIN pg_collation col ON col.oid=a.attcollation
          WHERE n.nspname=current_schema() AND c.relkind IN ('r','p')
            AND c.relname <> '_sqlx_migrations' AND a.attnum>0 AND NOT a.attisdropped
          UNION ALL
          SELECT 'constraint', jsonb_build_array(c.relname, x.conname, x.contype,
            pg_get_constraintdef(x.oid), x.condeferrable, x.condeferred, x.convalidated)
          FROM pg_constraint x JOIN pg_class c ON c.oid=x.conrelid
          JOIN pg_namespace n ON n.oid=c.relnamespace
          WHERE n.nspname=current_schema() AND c.relname <> '_sqlx_migrations'
          UNION ALL
          SELECT 'index', jsonb_build_array(c.relname, i.relname,
            replace(pg_get_indexdef(i.oid), current_schema() || '.', 'schema.'),
            x.indisvalid, x.indisready)
          FROM pg_index x JOIN pg_class c ON c.oid=x.indrelid
          JOIN pg_class i ON i.oid=x.indexrelid JOIN pg_namespace n ON n.oid=c.relnamespace
          WHERE n.nspname=current_schema() AND c.relname <> '_sqlx_migrations'
          UNION ALL
          SELECT 'sequence', jsonb_build_array(c.relname, format_type(s.seqtypid,NULL),
            s.seqstart,s.seqincrement,s.seqmax,s.seqmin,s.seqcache,s.seqcycle,
            owner.relname,a.attname,d.deptype)
          FROM pg_sequence s JOIN pg_class c ON c.oid=s.seqrelid
          JOIN pg_namespace n ON n.oid=c.relnamespace
          LEFT JOIN pg_depend d ON d.objid=c.oid AND d.classid='pg_class'::regclass
            AND d.refclassid='pg_class'::regclass AND d.deptype IN ('a','i')
          LEFT JOIN pg_class owner ON owner.oid=d.refobjid
          LEFT JOIN pg_attribute a ON a.attrelid=d.refobjid AND a.attnum=d.refobjsubid
          WHERE n.nspname=current_schema()
        ) q ORDER BY kind, body::text",
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    let sequences: Vec<String> = sqlx::query_scalar(
        "SELECT quote_ident(c.relname) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
         WHERE n.nspname=current_schema() AND c.relkind='S' ORDER BY c.relname",
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    for sequence in sequences {
        // quote_ident produced the identifier; values are never interpolated.
        let query = format!("SELECT jsonb_build_array(last_value,is_called)::text FROM {sequence}");
        let state: String = sqlx::query_scalar(sqlx::AssertSqlSafe(query))
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        records.push((format!("sequence_state:{sequence}"), state));
    }
    records
}

pub(super) async fn snapshot(pool: &PgPool) -> (Vec<(String, String)>, Vec<(String, String)>) {
    let mut conn = pool.acquire().await.unwrap();
    let mut structure = catalog(&mut conn).await;
    // Include object kinds that contain no rows (enums, composite types,
    // functions, and empty relations in other user schemas) in refusal proofs.
    let objects: Vec<(String,String)> = sqlx::query_as(
        "SELECT kind, body::text FROM (
            SELECT 'relation' AS kind, jsonb_build_array(n.nspname,c.relname,c.relkind) AS body
            FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
            WHERE n.nspname !~ '^pg_' AND n.nspname <> 'information_schema'
            UNION ALL
            SELECT 'type',jsonb_build_array(n.nspname,t.typname,t.typtype,e.enumlabel,e.enumsortorder)
            FROM pg_type t JOIN pg_namespace n ON n.oid=t.typnamespace
            LEFT JOIN pg_enum e ON e.enumtypid=t.oid
            WHERE n.nspname !~ '^pg_' AND n.nspname <> 'information_schema'
            UNION ALL
            SELECT 'function',jsonb_build_array(n.nspname,p.proname,pg_get_functiondef(p.oid))
            FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace
            WHERE n.nspname !~ '^pg_' AND n.nspname <> 'information_schema'
        ) q ORDER BY kind,body::text"
    ).fetch_all(&mut *conn).await.unwrap();
    structure.extend(objects);
    let relations: Vec<(String, String)> = sqlx::query_as(
        "SELECT quote_ident(n.nspname)||'.'||quote_ident(c.relname), c.relkind::text
         FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
         WHERE n.nspname !~ '^pg_' AND n.nspname <> 'information_schema'
           AND c.relkind IN ('r','S') ORDER BY 1",
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    let mut rows = Vec::new();
    for (name, kind) in relations {
        // Identifiers come from quote_ident, never untrusted query text.
        let query = if kind == "S" {
            format!("SELECT jsonb_build_array(last_value,is_called)::text FROM {name}")
        } else {
            format!("SELECT to_jsonb(t)::text FROM {name} t ORDER BY to_jsonb(t)::text")
        };
        let values: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(query.as_str()))
            .fetch_all(&mut *conn)
            .await
            .unwrap();
        rows.extend(values.into_iter().map(|v| (name.clone(), v)));
    }
    (structure, rows)
}

async fn reset(pool: &PgPool) {
    pool.execute("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        .await
        .unwrap();
}

#[sqlx::test(migrations = false)]
async fn initial_schema_matches_final_old_catalog(pool: PgPool) {
    let old = Migrator::new(std::path::Path::new(OLD_DIR)).await.unwrap();
    let new = Migrator::new(std::path::Path::new(NEW_DIR)).await.unwrap();
    let mut conn = pool.acquire().await.unwrap().detach();
    conn.execute("CREATE SCHEMA reference; SET search_path=reference")
        .await
        .unwrap();
    old.run(&mut conn).await.unwrap();
    // Apply only the owner's explicit fresh-schema amendments. Historical
    // fixtures remain immutable; all resulting catalog entries still compare.
    if !REFERENCE_AMENDMENT.is_empty() {
        conn.execute(REFERENCE_AMENDMENT).await.unwrap();
    }
    let reference = catalog(&mut conn).await;
    assert!(!reference.is_empty());
    conn.execute("CREATE SCHEMA candidate; SET search_path=candidate")
        .await
        .unwrap();
    new.run(&mut conn).await.unwrap();
    let candidate = catalog(&mut conn).await;
    assert_eq!(
        candidate, reference,
        "final columns, constraints, indexes, and sequences differ"
    );
    // Every non-ledger seed row must match. Normalize only the two generated
    // timestamps/UUID values whose values are intentionally fresh per database.
    let tables:Vec<String>=sqlx::query_scalar("SELECT quote_ident(tablename) FROM pg_tables WHERE schemaname='candidate' AND tablename <> '_sqlx_migrations' ORDER BY tablename")
        .fetch_all(&mut conn).await.unwrap();
    for table in tables {
        let projection = match table.as_str() {
            "field_types" => "to_jsonb(t)-'pinned_at'",
            "catalog_state" => "to_jsonb(t)-'catalog_id'",
            _ => "to_jsonb(t)",
        };
        let mut seeds = Vec::new();
        for schema in ["reference", "candidate"] {
            let query = format!(
                "SELECT ({projection})::text FROM {schema}.{table} t ORDER BY ({projection})::text"
            );
            let rows: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(query.as_str()))
                .fetch_all(&mut conn)
                .await
                .unwrap();
            seeds.push(rows);
        }
        assert_eq!(seeds[0], seeds[1], "seed mismatch in {table}");
    }
    conn.close().await.unwrap();
}

#[sqlx::test(migrations = false)]
async fn old_histories_are_refused_without_changes_and_release_locks(pool: PgPool) {
    let old = Migrator::new(std::path::Path::new(OLD_DIR)).await.unwrap();
    for (version, dirty) in [
        (old.iter().next().unwrap().version, false),
        (old.iter().last().unwrap().version, false),
        (old.iter().last().unwrap().version, true),
    ] {
        reset(&pool).await;
        old.run_to(version, &pool).await.unwrap();
        seed_old(&pool).await;
        if dirty {
            sqlx::query("UPDATE _sqlx_migrations SET success=false WHERE version=$1")
                .bind(version)
                .execute(&pool)
                .await
                .unwrap();
        }
        let before = snapshot(&pool).await;
        for _ in 0..2 {
            let error = tokio::time::timeout(Duration::from_secs(3), migrate(&pool))
                .await
                .expect("migration lock was released")
                .unwrap_err();
            assert!(
                matches!(error, SchemaError::LegacyHistory { .. }),
                "{error:?}"
            );
            assert!(error.to_string().contains("retain the old database"));
            assert_eq!(
                snapshot(&pool).await,
                before,
                "refusal changed old database"
            );
        }
    }
}

#[sqlx::test(migrations = false)]
async fn untracked_objects_are_refused_without_creating_history(pool: PgPool) {
    for ddl in [
        "CREATE TABLE sentinel (id BIGSERIAL PRIMARY KEY, payload TEXT); INSERT INTO sentinel(payload) VALUES ('retain me')",
        "CREATE TYPE sentinel AS ENUM ('retain_me')",
        "CREATE TYPE sentinel AS (payload TEXT)",
        "CREATE FUNCTION sentinel() RETURNS integer LANGUAGE sql AS 'SELECT 42'",
        "CREATE SCHEMA other; CREATE TABLE other.sentinel(payload TEXT)",
    ] {
        reset(&pool).await;
        pool.execute(ddl).await.unwrap();
        let before = snapshot(&pool).await;
        for _ in 0..2 {
            let error = tokio::time::timeout(Duration::from_secs(3), migrate(&pool))
                .await
                .unwrap()
                .unwrap_err();
            assert!(matches!(error, SchemaError::UntrackedSchema), "{error:?}");
            assert_eq!(snapshot(&pool).await, before);
            let ledger: bool =
                sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert!(!ledger, "refusal created migration ledger");
        }
    }
}

#[sqlx::test(migrations = false)]
async fn current_checksums_dirty_and_unknown_versions_stay_distinct(pool: PgPool) {
    for (update, expected) in [
        (
            "UPDATE _sqlx_migrations SET checksum=decode('00','hex')",
            "checksum",
        ),
        ("UPDATE _sqlx_migrations SET success=false", "dirty"),
        ("UPDATE _sqlx_migrations SET version=42", "unknown"),
        (
            "UPDATE _sqlx_migrations SET version=20260914000001",
            "unknown",
        ),
    ] {
        reset(&pool).await;
        migrate(&pool).await.unwrap();
        pool.execute(update).await.unwrap();
        let before = snapshot(&pool).await;
        let error = tokio::time::timeout(Duration::from_secs(3), migrate(&pool))
            .await
            .unwrap()
            .unwrap_err();
        assert!(
            matches!(
                (&error, expected),
                (
                    SchemaError::Migration(MigrateError::VersionMismatch(_)),
                    "checksum"
                ) | (SchemaError::Migration(MigrateError::Dirty(_)), "dirty")
                    | (
                        SchemaError::Migration(MigrateError::VersionMissing(_)),
                        "unknown"
                    )
            ),
            "{error:?}"
        );
        assert_eq!(snapshot(&pool).await, before);
    }
}

#[sqlx::test(migrations = false)]
async fn fresh_restart_preserves_history_and_forward_migrations_work(pool: PgPool) {
    migrate(&pool).await.unwrap();
    let before = snapshot(&pool).await;
    migrate(&pool).await.unwrap();
    assert_eq!(snapshot(&pool).await, before);
    let directory = tempfile::tempdir().unwrap();
    for entry in std::fs::read_dir(NEW_DIR).unwrap() {
        let path = entry.unwrap().path();
        std::fs::copy(&path, directory.path().join(path.file_name().unwrap())).unwrap();
    }
    std::fs::write(
        directory
            .path()
            .join(format!("{}_forward_fixture.sql", BASELINE + 1)),
        "CREATE TABLE forward_fixture (value INTEGER NOT NULL);",
    )
    .unwrap();
    let future = Migrator::new(directory.path()).await.unwrap();
    future.run(&pool).await.unwrap();
    let after = snapshot(&pool).await;
    future.run(&pool).await.unwrap();
    assert_eq!(snapshot(&pool).await, after);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE version=$1")
        .bind(BASELINE + 1)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[sqlx::test(migrations = false)]
async fn cancelled_lock_wait_does_not_recycle_a_locked_connection(pool: PgPool) {
    let mut blocker = pool.acquire().await.unwrap().detach();
    blocker.lock().await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), migrate(&pool))
            .await
            .is_err()
    );
    blocker.close().await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), migrate(&pool))
        .await
        .expect("cancelled connection released lock")
        .unwrap();
}

/// An unlocked emptiness probe is not authority: another migration-lock holder
/// can publish application state while a runner waits for that same lock.
#[sqlx::test(migrations = false)]
async fn admission_observes_state_created_while_waiting_for_the_lock(pool: PgPool) {
    let mut blocker = pool.acquire().await.unwrap().detach();
    blocker.lock().await.unwrap();
    let pending = migrate(&pool);
    tokio::pin!(pending);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut pending)
            .await
            .is_err()
    );
    blocker
        .execute("CREATE TABLE sentinel(payload TEXT); INSERT INTO sentinel VALUES ('retain me')")
        .await
        .unwrap();
    let before = snapshot(&pool).await;
    blocker.close().await.unwrap();
    let error = tokio::time::timeout(Duration::from_secs(3), pending)
        .await
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, SchemaError::UntrackedSchema), "{error:?}");
    assert_eq!(snapshot(&pool).await, before);
}
