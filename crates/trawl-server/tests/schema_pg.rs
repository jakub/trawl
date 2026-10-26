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
CREATE TABLE investigations (
    id TEXT NOT NULL, title TEXT NOT NULL, question TEXT NOT NULL, scope TEXT NOT NULL,
    status TEXT NOT NULL CONSTRAINT investigations_status_check
        CHECK (status IN ('open', 'closed')),
    seq BIGINT NOT NULL, created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ NOT NULL,
    retain_until TIMESTAMPTZ NOT NULL, retain_ceiling TIMESTAMPTZ NOT NULL,
    created_by_key_id BIGINT NOT NULL, created_by_prefix TEXT NOT NULL,
    created_by_name TEXT NOT NULL,
    updated_by_key_id BIGINT NOT NULL, updated_by_prefix TEXT NOT NULL,
    updated_by_name TEXT NOT NULL,
    creation TEXT NOT NULL,
    CONSTRAINT investigations_pkey PRIMARY KEY (id),
    CONSTRAINT investigations_id_hex CHECK (id ~ '^[0-9a-f]{32}$'),
    CONSTRAINT investigations_seq CHECK (seq >= 0),
    CONSTRAINT investigations_created_by_prefix_len
        CHECK (octet_length(created_by_prefix) <= 8),
    CONSTRAINT investigations_updated_by_prefix_len
        CHECK (octet_length(updated_by_prefix) <= 8),
    CONSTRAINT investigations_title_len CHECK (octet_length(title) <= 200),
    CONSTRAINT investigations_question_len CHECK (octet_length(question) <= 2048),
    CONSTRAINT investigations_scope_len CHECK (octet_length(scope) <= 2048),
    CONSTRAINT investigations_creation_len CHECK (octet_length(creation) <= 32768),
    CONSTRAINT investigations_retention CHECK (retain_until <= retain_ceiling)
);
CREATE INDEX investigations_listing ON investigations (status DESC, updated_at DESC);
CREATE TABLE investigation_log (
    investigation_id TEXT NOT NULL, seq BIGINT NOT NULL, updated_seq BIGINT NOT NULL,
    kind TEXT NOT NULL CONSTRAINT investigation_log_kind_check
        CHECK (kind IN ('step', 'keep', 'finding', 'finding_status', 'note', 'status',
                        'title', 'question')),
    actor_key_id BIGINT NOT NULL, actor_prefix TEXT NOT NULL, actor_name TEXT NOT NULL,
    at TIMESTAMPTZ NOT NULL,
    payload TEXT NOT NULL,
    outcome TEXT CONSTRAINT investigation_log_outcome_check
        CHECK (outcome IN ('success', 'refused', 'timeout', 'failed', 'aborted',
                           'interrupted')),
    outcome_code TEXT, result_id TEXT,
    CONSTRAINT investigation_log_pkey PRIMARY KEY (investigation_id, seq),
    CONSTRAINT investigation_log_investigation_fk FOREIGN KEY (investigation_id)
        REFERENCES investigations (id) ON DELETE CASCADE,
    CONSTRAINT investigation_log_actor_prefix_len CHECK (octet_length(actor_prefix) <= 8),
    CONSTRAINT investigation_log_seq CHECK (seq > 0 AND updated_seq >= seq),
    CONSTRAINT investigation_log_payload_len CHECK (octet_length(payload) <= 524288),
    CONSTRAINT investigation_log_step_columns CHECK (
        kind = 'step' OR (outcome IS NULL AND outcome_code IS NULL AND result_id IS NULL)),
    CONSTRAINT investigation_log_outcome_facts CHECK (
        outcome IS NOT NULL OR (outcome_code IS NULL AND result_id IS NULL)),
    CONSTRAINT investigation_log_result_hex CHECK (
        result_id IS NULL OR result_id ~ '^[0-9a-f]{32}$')
);
CREATE INDEX investigation_log_updated ON investigation_log (investigation_id, updated_seq);
CREATE INDEX investigation_log_running ON investigation_log (investigation_id, seq)
    WHERE kind = 'step' AND outcome IS NULL;
ALTER TABLE captured_results
    ALTER COLUMN investigation_id TYPE TEXT USING NULL,
    ADD CONSTRAINT captured_results_investigation_fk FOREIGN KEY (investigation_id)
        REFERENCES investigations (id) ON DELETE SET NULL;
CREATE INDEX captured_results_investigation ON captured_results (investigation_id, state)
    WHERE investigation_id IS NOT NULL;
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
