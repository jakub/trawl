//! Fresh schema equivalence and admission through the operational runner.
use sqlx::PgPool;
use trawl_server::store::migrations::{BASELINE_VERSION, SchemaError, migrate};
const BASELINE: i64 = BASELINE_VERSION;
const OLD_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../scripts/schema-baseline/fixtures/trawl"
);
const NEW_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations");
// Intentional fresh-schema differences from the immutable historical fixtures.
// Keep these statements explicit so the remaining catalog comparison is strict.
const REFERENCE_AMENDMENT: &str = "
ALTER TABLE catalog_state DROP COLUMN services_backfilled_at;
ALTER TABLE repin_jobs
    ADD CONSTRAINT repin_jobs_accepted_nulled_plan
    CHECK ((accepted_max_nulled_rows IS NOT NULL) = (force AND planned_at IS NOT NULL)),
    ADD CONSTRAINT repin_jobs_accepted_ambiguous_plan
    CHECK ((accepted_max_ambiguous_rows IS NOT NULL) = (force AND planned_at IS NOT NULL)),
    ADD CONSTRAINT repin_jobs_requested_force
    CHECK (force OR (max_nulled_rows IS NULL AND max_ambiguous_rows IS NULL));
ALTER TABLE report_runs
    ADD COLUMN origin TEXT,
    ADD CONSTRAINT report_runs_origin
    CHECK (origin IS NULL OR origin IN ('scheduled', 'manual'));
ALTER TABLE repin_jobs
    ADD COLUMN requested_by_key_id BIGINT;
CREATE TABLE captured_results (
    id TEXT NOT NULL, key_id BIGINT NOT NULL, created_at TIMESTAMPTZ NOT NULL,
    expires_at TIMESTAMPTZ, gone_at TIMESTAMPTZ,
    state TEXT NOT NULL CONSTRAINT captured_results_state_check
        CHECK (state IN ('temporary', 'kept', 'expired', 'pruned')),
    query TEXT NOT NULL, executed_query TEXT NOT NULL,
    source TEXT NOT NULL CONSTRAINT captured_results_source_check
        CHECK (source IN ('live', 'stored')),
    shape TEXT NOT NULL CONSTRAINT captured_results_shape_check
        CHECK (shape IN ('events', 'whole')),
    earliest TIMESTAMPTZ, latest TIMESTAMPTZ,
    columns TEXT NOT NULL, column_facts TEXT NOT NULL,
    captured_rows BIGINT NOT NULL, total_rows BIGINT NOT NULL,
    wire_bytes BIGINT NOT NULL, file_bytes BIGINT NOT NULL, digest BYTEA,
    file_missing BOOLEAN NOT NULL DEFAULT FALSE, format_version SMALLINT NOT NULL,
    investigation_id BIGINT,
    CONSTRAINT captured_results_pkey PRIMARY KEY (id),
    CONSTRAINT captured_results_id_hex CHECK (id ~ '^[0-9a-f]{32}$'),
    CONSTRAINT captured_results_deadline_state CHECK ((expires_at IS NULL) = (state = 'kept')),
    CONSTRAINT captured_results_gone_state CHECK (
        (gone_at IS NOT NULL) = (state IN ('expired', 'pruned'))),
    CONSTRAINT captured_results_bounds CHECK (
        CASE WHEN source = 'stored' THEN earliest IS NULL AND latest IS NULL
             ELSE earliest IS NOT NULL AND latest IS NOT NULL AND earliest <= latest
        END),
    CONSTRAINT captured_results_rows CHECK (captured_rows >= 0 AND captured_rows <= total_rows),
    CONSTRAINT captured_results_sizes CHECK (wire_bytes >= 0 AND file_bytes >= 0),
    CONSTRAINT captured_results_digest_iff_file CHECK ((digest IS NULL) = (file_bytes = 0)),
    CONSTRAINT captured_results_digest_len CHECK (digest IS NULL OR octet_length(digest) = 32),
    CONSTRAINT captured_results_format_version CHECK (format_version > 0)
);
CREATE INDEX captured_results_key_live ON captured_results (key_id, expires_at)
    WHERE state = 'temporary';
CREATE INDEX captured_results_sweep ON captured_results (state, expires_at);
CREATE INDEX captured_results_gone ON captured_results (gone_at) WHERE gone_at IS NOT NULL;
ALTER TABLE captured_results
    ADD COLUMN kind TEXT NOT NULL DEFAULT 'search' CONSTRAINT captured_results_kind_check
        CHECK (kind IN ('search', 'context', 'sightings', 'compare')),
    ADD COLUMN correlation TEXT,
    ADD COLUMN metadata_bytes BIGINT NOT NULL DEFAULT 0
        CONSTRAINT captured_results_metadata_bytes CHECK (metadata_bytes >= 0);
";
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
