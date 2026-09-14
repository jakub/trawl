-- Initial Trawl application schema. Owned by trawld under its sole-writer lock.
-- This baseline requires a fresh database. Append future migrations; do not
-- edit this file after it has been applied. Constraint names are part of the
-- store error contract, alongside SQLSTATE.

-- Fleet key IDs cross the database boundary by value. The scheduler checks
-- key liveness; PostgreSQL cannot enforce a cross-database foreign key.

CREATE TABLE query_history (
    id          BIGSERIAL PRIMARY KEY,
    key_id      BIGINT      NOT NULL,
    query       TEXT        NOT NULL,
    executed_at TIMESTAMPTZ NOT NULL,
    duration_ms BIGINT      NOT NULL,
    row_count   BIGINT      NOT NULL,
    status      TEXT        NOT NULL
        CONSTRAINT query_history_status_check
        CHECK (status IN ('success', 'error', 'timeout'))
);

CREATE TABLE saved_queries (
    id         BIGSERIAL PRIMARY KEY,
    key_id     BIGINT      NOT NULL,
    name       TEXT        NOT NULL,
    query      TEXT        NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    CONSTRAINT saved_queries_key_name_unique UNIQUE (key_id, name)
);

-- The writer supplies next_fire_at; it has no database default. A NULL window
-- mode remains unwindowed. CASE checks reject partial shapes instead of letting
-- SQL NULL truth values satisfy them. Durations are capped at ten years.

CREATE TABLE schedules (
    id             BIGSERIAL PRIMARY KEY,
    saved_query_id BIGINT      NOT NULL
        CONSTRAINT schedules_saved_query_unique UNIQUE
        CONSTRAINT schedules_saved_query_fk
        REFERENCES saved_queries (id) ON DELETE CASCADE,
    key_id         BIGINT      NOT NULL,
    interval_secs  BIGINT      NOT NULL
        CONSTRAINT schedules_interval_min CHECK (interval_secs >= 60),
    max_runs       BIGINT,
    enabled        BOOLEAN     NOT NULL DEFAULT TRUE,
    created_at     TIMESTAMPTZ NOT NULL,
    updated_at     TIMESTAMPTZ NOT NULL,
    window_kind     TEXT,
    window_secs     BIGINT,
    lag_secs        BIGINT NOT NULL DEFAULT 0,
    covered_through TIMESTAMPTZ,
    next_fire_at    TIMESTAMPTZ NOT NULL,
    CONSTRAINT schedules_window_kind CHECK (
        window_kind IS NULL OR window_kind IN ('since_last', 'fixed')),
    CONSTRAINT schedules_window_shape CHECK (
        CASE WHEN window_kind IS NULL         THEN window_secs IS NULL
             WHEN window_kind = 'since_last'  THEN window_secs IS NULL
             WHEN window_kind = 'fixed'       THEN window_secs IS NOT NULL
                                                   AND window_secs >= 60
             ELSE FALSE
        END),
    CONSTRAINT schedules_lag_nonneg CHECK (lag_secs >= 0),
    CONSTRAINT schedules_interval_within_cap CHECK (interval_secs <= 315360000),
    CONSTRAINT schedules_window_secs_within_cap CHECK (
        window_secs IS NULL OR window_secs <= 315360000),
    CONSTRAINT schedules_lag_within_cap CHECK (lag_secs <= 315360000)
);

-- Runs snapshot their claimed window mode. Result bytes are the zstd-JSON
-- fallback when Parquet fails; result_path is relative to the data directory.

CREATE TABLE report_runs (
    id             BIGSERIAL PRIMARY KEY,
    schedule_id    BIGINT      NOT NULL
        CONSTRAINT report_runs_schedule_fk
        REFERENCES schedules (id) ON DELETE CASCADE,
    saved_query_id BIGINT      NOT NULL
        CONSTRAINT report_runs_saved_query_fk
        REFERENCES saved_queries (id) ON DELETE CASCADE,
    query          TEXT        NOT NULL,
    status         TEXT        NOT NULL
        CONSTRAINT report_runs_status_check
        CHECK (status IN ('running', 'success', 'error', 'timeout')),
    started_at     TIMESTAMPTZ NOT NULL,
    finished_at    TIMESTAMPTZ,
    duration_ms    BIGINT,
    row_count      BIGINT,
    error_message  TEXT,
    result_data    BYTEA,
    result_path    TEXT,
    window_start     TIMESTAMPTZ,
    window_end       TIMESTAMPTZ,
    window_truncated BOOLEAN,
    window_kind      TEXT,
    CONSTRAINT report_runs_window_kind CHECK (
        window_kind IS NULL OR window_kind IN ('since_last', 'fixed')),
    CONSTRAINT report_runs_window_shape CHECK (
        CASE WHEN window_start IS NULL
             THEN window_end IS NULL AND window_truncated IS NULL AND window_kind IS NULL
             ELSE window_end IS NOT NULL AND window_truncated IS NOT NULL
                  AND window_kind IS NOT NULL AND window_start < window_end
        END)
);

-- Global write-time pins, bounded by the store pin cap. Retention does not
-- delete pins. SEVERITY is a semantic type stored physically as BIGINT.

CREATE TABLE field_types (
    field       TEXT
        CONSTRAINT field_types_pkey PRIMARY KEY,
    duckdb_type TEXT        NOT NULL,
    pinned_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    pinned_from TEXT,
    CONSTRAINT field_types_type_check
    CHECK (duckdb_type IN ('BOOLEAN', 'BIGINT', 'DOUBLE', 'TIMESTAMP', 'VARCHAR', 'SEVERITY'))
);

-- Ever-observed service fields. Consumers window on last_seen; retention
-- does not reconcile observations against currently retained files.

CREATE TABLE field_services (
    field      TEXT        NOT NULL,
    service    TEXT        NOT NULL,
    first_seen TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen  TIMESTAMPTZ NOT NULL DEFAULT now(),
    row_count  BIGINT      NOT NULL DEFAULT 0,
    CONSTRAINT field_services_pkey PRIMARY KEY (field, service)
);

-- Bounded lossy-cast evidence. Sample count/byte limits belong to the writer
-- so malformed evidence cannot prevent compaction bookkeeping.

CREATE TABLE field_conflicts (
    id            BIGSERIAL
        CONSTRAINT field_conflicts_pkey PRIMARY KEY,
    field         TEXT        NOT NULL,
    service       TEXT        NOT NULL,
    observed_type TEXT        NOT NULL,
    expected_type TEXT        NOT NULL,
    rows_nulled   BIGINT      NOT NULL,
    at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    samples TEXT[] NOT NULL DEFAULT '{}'
);

-- One catalog identity, mirrored by the data-root CATALOG marker. Both
-- completion instants remain NULL until their boot passes actually complete.

CREATE TABLE catalog_state (
    singleton    BOOLEAN     NOT NULL DEFAULT TRUE
        CONSTRAINT catalog_state_pkey PRIMARY KEY
        CONSTRAINT catalog_state_singleton CHECK (singleton),
    catalog_id   UUID        NOT NULL DEFAULT gen_random_uuid(),
    -- Completion covers file conformance and service observations together.
    conformed_at TIMESTAMPTZ
);

-- One job is both the scan report and the rewrite lifecycle. planned_at NULL
-- means no plan was recorded. Requested/accepted force ceilings are nullable.
-- Cancellation requires a paired request; a crash after a request is failed,
-- never inferred cancelled. Staged tally arrays are both NULL until staging;
-- empty arrays prove a lossless result. A dry run cannot stage rewrite tallies.

CREATE TABLE repin_jobs (
    id            BIGSERIAL
        CONSTRAINT repin_jobs_pkey PRIMARY KEY,
    field         TEXT        NOT NULL,
    from_type     TEXT        NOT NULL,
    to_type       TEXT        NOT NULL,
    dry_run       BOOLEAN     NOT NULL,
    force         BOOLEAN     NOT NULL DEFAULT FALSE,
    status        TEXT        NOT NULL DEFAULT 'running',
    requested_by  TEXT,
    started_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at   TIMESTAMPTZ,
    error         TEXT,
    files_total     BIGINT NOT NULL DEFAULT 0,
    rows_carrying   BIGINT NOT NULL DEFAULT 0,
    projected_nulls BIGINT NOT NULL DEFAULT 0,
    resurrectable   BIGINT NOT NULL DEFAULT 0,
    affected_bytes  BIGINT NOT NULL DEFAULT 0,
    files_done       BIGINT NOT NULL DEFAULT 0,
    rows_rewritten   BIGINT NOT NULL DEFAULT 0,
    rows_nulled      BIGINT NOT NULL DEFAULT 0,
    rows_resurrected BIGINT NOT NULL DEFAULT 0,
    CONSTRAINT repin_jobs_from_type_check
    CHECK (from_type IN ('BOOLEAN', 'BIGINT', 'DOUBLE', 'TIMESTAMP',
                         'VARCHAR', 'SEVERITY')),
    CONSTRAINT repin_jobs_to_type_check
    CHECK (to_type IN ('BOOLEAN', 'BIGINT', 'DOUBLE', 'TIMESTAMP',
                       'VARCHAR', 'SEVERITY')),
    dialect TEXT,
    CONSTRAINT repin_jobs_dialect_check
    CHECK (dialect IN ('otel', 'syslog')),
    CONSTRAINT repin_jobs_dialect_scope_check
    CHECK ((dialect IS NOT NULL) = (to_type = 'SEVERITY')),
    ambiguous_numerals BIGINT NOT NULL DEFAULT 0,
    unmapped_samples TEXT[] NOT NULL DEFAULT '{}',
    field_last_seen TIMESTAMPTZ,
    field_last_service TEXT,
    planned_at TIMESTAMPTZ,
    CONSTRAINT repin_jobs_status_check
    CHECK (status IN ('running', 'succeeded', 'failed',
                      'refused_needs_force', 'blocked', 'cancelled')),
    cancel_requested_at TIMESTAMPTZ,
    cancelled_by TEXT,
    CONSTRAINT repin_jobs_cancel_request_check
    CHECK ((cancel_requested_at IS NULL) = (cancelled_by IS NULL)),
    CONSTRAINT repin_jobs_cancelled_request_check
    CHECK (status <> 'cancelled' OR cancel_requested_at IS NOT NULL),
    max_nulled_rows BIGINT,
    CONSTRAINT repin_jobs_max_nulled_rows_check
    CHECK (max_nulled_rows >= 0),
    max_ambiguous_rows BIGINT,
    CONSTRAINT repin_jobs_max_ambiguous_rows_check
    CHECK (max_ambiguous_rows >= 0),
    accepted_max_nulled_rows BIGINT,
    CONSTRAINT repin_jobs_accepted_max_nulled_rows_check
    CHECK (accepted_max_nulled_rows >= 0),
    accepted_max_ambiguous_rows BIGINT,
    CONSTRAINT repin_jobs_accepted_max_ambiguous_rows_check
    CHECK (accepted_max_ambiguous_rows >= 0),
    -- Claimed/interrupted jobs can lack a plan. A measured forced plan
    -- always records both bounds; unforced plans record neither.
    CONSTRAINT repin_jobs_accepted_nulled_plan
    CHECK ((accepted_max_nulled_rows IS NOT NULL) = (force AND planned_at IS NOT NULL)),
    CONSTRAINT repin_jobs_accepted_ambiguous_plan
    CHECK ((accepted_max_ambiguous_rows IS NOT NULL) = (force AND planned_at IS NOT NULL)),
    CONSTRAINT repin_jobs_requested_force
    CHECK (force OR (max_nulled_rows IS NULL AND max_ambiguous_rows IS NULL)),
    nulled_services     TEXT[],
    nulled_service_rows BIGINT[],
    CONSTRAINT repin_jobs_nulled_tallies_paired_check
    CHECK ((nulled_services IS NULL) = (nulled_service_rows IS NULL)),
    CONSTRAINT repin_jobs_nulled_tallies_cardinality_check
    CHECK (cardinality(nulled_services) = cardinality(nulled_service_rows)),
    CONSTRAINT repin_jobs_nulled_tallies_scope_check
    CHECK (nulled_services IS NULL OR NOT dry_run)
);

-- Durable ever-observed aggregates, independent of the bounded detail window.
-- A successful repin clears evidence for the pin inside the cutover transaction.

CREATE TABLE field_conflict_stats (
    field             TEXT        NOT NULL,
    service           TEXT        NOT NULL,
    first_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    episodes          BIGINT      NOT NULL DEFAULT 0,
    rows_nulled_total BIGINT      NOT NULL DEFAULT 0,
    CONSTRAINT field_conflict_stats_pkey PRIMARY KEY (field, service)
);

-- Acknowledgement covers an episode high-water, not a timestamp. New evidence
-- raises the badge again. The FK protects pin lifetime; repin removes the ack.

CREATE TABLE field_degraded_ack (
    field            TEXT
        CONSTRAINT field_degraded_ack_pkey PRIMARY KEY
        CONSTRAINT field_degraded_ack_field_fkey
            REFERENCES field_types (field) ON DELETE CASCADE,
    acked_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    acked_by         TEXT        NOT NULL,
    note             TEXT
        CONSTRAINT field_degraded_ack_note_length_check
        CHECK (octet_length(note) <= 1024),
    evidence_through BIGINT      NOT NULL
        CONSTRAINT field_degraded_ack_evidence_through_check
        CHECK (evidence_through >= 0)
);

-- Cursor indexes retain their tie-break direction and covering columns.
-- Partial unique indexes enforce one running report per schedule and one
-- running repin for the entire installation, including concurrent claims.

CREATE INDEX query_history_key_executed_idx
    ON query_history (key_id, executed_at DESC, id DESC);

CREATE INDEX schedules_enabled_idx ON schedules (enabled);

CREATE INDEX report_runs_schedule_started_idx
    ON report_runs (schedule_id, started_at DESC, id DESC);

CREATE INDEX report_runs_saved_query_idx
    ON report_runs (saved_query_id, started_at DESC, id DESC);

CREATE UNIQUE INDEX report_runs_one_running
    ON report_runs (schedule_id) WHERE status = 'running';

CREATE INDEX field_conflicts_field_at_idx
    ON field_conflicts (field, at DESC);

CREATE INDEX field_services_service_field_idx
    ON field_services (service, field)
    INCLUDE (row_count, first_seen, last_seen);

CREATE INDEX field_services_field_last_seen_idx
    ON field_services (field, last_seen DESC, service)
    INCLUDE (first_seen, row_count);

CREATE INDEX field_conflicts_at_idx
    ON field_conflicts (at DESC, id DESC);

CREATE UNIQUE INDEX repin_jobs_one_running
    ON repin_jobs ((TRUE)) WHERE status = 'running';

CREATE INDEX repin_jobs_started_idx
    ON repin_jobs (started_at DESC, id DESC);

INSERT INTO catalog_state (singleton) VALUES (TRUE);

-- Must mirror trawl_core::schema::ENVELOPE_TYPES. No roles or data are adopted.
INSERT INTO field_types (field, duckdb_type, pinned_from) VALUES
    ('_time', 'TIMESTAMP', '_declared'),
    ('_ingested', 'TIMESTAMP', '_declared'),
    ('_raw', 'VARCHAR', '_declared'),
    ('_repairs', 'VARCHAR', '_declared'),
    ('env', 'VARCHAR', '_declared'),
    ('service', 'VARCHAR', '_declared'),
    ('host', 'VARCHAR', '_declared'),
    ('message', 'VARCHAR', '_declared'),
    ('_severity', 'SEVERITY', '_declared'),
    ('_producer', 'VARCHAR', '_declared');
