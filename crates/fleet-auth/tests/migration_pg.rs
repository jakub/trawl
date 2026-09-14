//! Fresh baseline, refusal, and forward-migration contracts on owned PostgreSQL.
#![cfg(feature = "keystore")]
use fleet_auth::{SchemaError, migrate};
use sqlx::PgPool;
const BASELINE: i64 = fleet_auth::migrations::BASELINE_VERSION;
const OLD_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../scripts/schema-baseline/fixtures/fleet"
);
const NEW_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations");
const REFERENCE_AMENDMENT: &str = "";
async fn seed_old(pool: &PgPool) {
    sqlx::query("INSERT INTO api_keys(prefix,name,hash,kind) VALUES ('oldkey01','retained key','$argon2id$fixture','service')")
        .execute(pool).await.unwrap();
}
#[path = "../../../scripts/schema-baseline/cases.rs"]
mod cases;

#[sqlx::test(migrations = false)]
async fn runtime_validation_is_read_only_and_requires_current_history(pool: PgPool) {
    assert!(matches!(
        fleet_auth::validate_schema(&pool).await,
        Err(SchemaError::Uninitialized)
    ));
    let ledger: bool = sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!ledger);
    migrate(&pool).await.unwrap();
    fleet_auth::validate_schema(&pool).await.unwrap();
    sqlx::query("UPDATE _sqlx_migrations SET checksum=decode('00','hex')")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        fleet_auth::validate_schema(&pool).await,
        Err(SchemaError::Migration(
            sqlx::migrate::MigrateError::VersionMismatch(_)
        ))
    ));
    let success: bool =
        sqlx::query_scalar("SELECT checksum=decode('00','hex') FROM _sqlx_migrations")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(success);
}

#[sqlx::test(migrations = false)]
async fn fresh_registry_grants_nothing(pool: PgPool) {
    migrate(&pool).await.unwrap();
    for table in ["api_keys", "roles", "key_roles", "role_permissions"] {
        let count: i64 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 0, "unexpected initial grants in {table}");
    }
    let permissions: Vec<String> = sqlx::query_scalar(
        "SELECT permission FROM app_permissions WHERE app='trawl' ORDER BY permission",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        permissions,
        [
            "export",
            "ingest",
            "query",
            "query_cancel",
            "saved_query",
            "schema_read",
            "schema_write",
            "server_manage",
            "stream",
            "validate"
        ]
    );
}

/// Runtime callers cannot bypass the fresh baseline by omitting migration.
#[sqlx::test(migrations = false)]
async fn runtime_connect_rejects_old_and_invalid_histories_without_writes(pool: PgPool) {
    use sqlx::{ConnectOptions as _, Executor as _};
    let old = sqlx::migrate::Migrator::new(std::path::Path::new(OLD_DIR))
        .await
        .unwrap();
    let url = pool.connect_options().to_url_lossy();
    for state in ["old", "dirty", "unknown", "checksum", "untracked"] {
        pool.execute("DROP SCHEMA public CASCADE; CREATE SCHEMA public")
            .await
            .unwrap();
        if state == "old" {
            old.run(&pool).await.unwrap();
            seed_old(&pool).await;
        } else if state == "untracked" {
            pool.execute("CREATE TABLE sentinel(payload TEXT)")
                .await
                .unwrap();
        } else {
            migrate(&pool).await.unwrap();
            let update = match state {
                "dirty" => "UPDATE _sqlx_migrations SET success=false",
                "unknown" => "UPDATE _sqlx_migrations SET version=20260914000001",
                _ => "UPDATE _sqlx_migrations SET checksum=decode('00','hex')",
            };
            pool.execute(update).await.unwrap();
        }
        let before = cases::snapshot(&pool).await;
        let error = fleet_auth::validate_schema(&pool).await.unwrap_err();
        assert!(
            matches!(
                (&error, state),
                (SchemaError::LegacyHistory { .. }, "old")
                    | (SchemaError::UntrackedSchema, "untracked")
                    | (
                        SchemaError::Migration(sqlx::migrate::MigrateError::Dirty(_)),
                        "dirty"
                    )
                    | (
                        SchemaError::Migration(sqlx::migrate::MigrateError::VersionMissing(_)),
                        "unknown"
                    )
                    | (
                        SchemaError::Migration(sqlx::migrate::MigrateError::VersionMismatch(_)),
                        "checksum"
                    )
            ),
            "{error:?}"
        );
        assert!(matches!(
            fleet_auth::KeyStore::connect(url.as_str()).await,
            Err(fleet_auth::AuthError::Schema(_))
        ));
        assert_eq!(cases::snapshot(&pool).await, before);
    }
    pool.execute("DROP SCHEMA public CASCADE; CREATE SCHEMA public")
        .await
        .unwrap();
    migrate(&pool).await.unwrap();
    let store = fleet_auth::KeyStore::connect(url.as_str()).await.unwrap();
    let created = store
        .create_key("restart", fleet_auth::PrincipalKind::Service, &[], None)
        .await
        .unwrap();
    let token = created.plaintext_token;
    drop(store);
    let reopened = fleet_auth::KeyStore::connect(url.as_str()).await.unwrap();
    reopened.verify_key(&token).await.unwrap();
}
