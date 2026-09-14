//! Fresh schema equivalence and admission through the operational runner.
use sqlx::PgPool;
use trawl_server::store::migrations::{BASELINE_VERSION, SchemaError, migrate};
const BASELINE: i64 = BASELINE_VERSION;
const OLD_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../scripts/schema-baseline/fixtures/trawl"
);
const NEW_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations");
async fn seed_old(pool: &PgPool) {
    sqlx::query("INSERT INTO saved_queries(key_id,name,query,created_at,updated_at) VALUES (1,'retained query','service=test',now(),now())")
        .execute(pool).await.unwrap();
}
#[path = "../../../scripts/schema-baseline/cases.rs"]
mod cases;

#[sqlx::test(migrations = false)]
async fn failed_boot_retains_old_data_and_releases_the_sole_writer_lock(pool: PgPool) {
    use trawl_server::store::{StorageState, StoreError};
    let old = sqlx::migrate::Migrator::new(std::path::Path::new(OLD_DIR))
        .await
        .unwrap();
    old.run(&pool).await.unwrap();
    seed_old(&pool).await;
    let before = cases::snapshot(&pool).await;
    for _ in 0..2 {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                // The shared pool's target stays fixed for the entire test.
                match StorageState::from_pool(pool.clone()).await {
                    Err(StoreError::Migration(SchemaError::LegacyHistory { .. })) => break,
                    Err(StoreError::LockHeld) => tokio::task::yield_now().await,
                    Err(error) => panic!("unexpected failed-boot result: {error}"),
                    Ok(_) => panic!("old history booted"),
                }
            }
        })
        .await
        .expect("failed boot released its sole-writer lock");
        assert_eq!(cases::snapshot(&pool).await, before);
    }
}
