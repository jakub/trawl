-- trawl app-state store: query history, saved queries, schedules, report
-- runs (ADR-0004 slice 3). Owned and boot-migrated by trawld — the sole
-- writer, enforced by a session advisory lock taken before migration.
--
-- key_id is a fleet keystore id carried BY VALUE: fleet keys live in the
-- separate `fleet` database and postgres cannot enforce cross-database FKs.
-- The scheduler's per-tick `get_live_key_by_id` liveness gate is the
-- integrity mechanism.
--
-- Every constraint is NAMED: trawl-server's store error mapping keys on
-- (SQLSTATE, constraint name) — never on message text.

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

-- Pagination is ORDER BY executed_at DESC, id DESC (id as tie-break for
-- deterministic order on same-timestamp inserts).
CREATE INDEX query_history_key_executed_idx
    ON query_history (key_id, executed_at DESC, id DESC);

CREATE TABLE saved_queries (
    id         BIGSERIAL PRIMARY KEY,
    key_id     BIGINT      NOT NULL,
    name       TEXT        NOT NULL,
    query      TEXT        NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,

    CONSTRAINT saved_queries_key_name_unique UNIQUE (key_id, name)
);

CREATE TABLE schedules (
    id             BIGSERIAL PRIMARY KEY,
    saved_query_id BIGINT      NOT NULL
        CONSTRAINT schedules_saved_query_unique UNIQUE
        CONSTRAINT schedules_saved_query_fk
        REFERENCES saved_queries (id) ON DELETE CASCADE,
    key_id         BIGINT      NOT NULL,
    interval_secs  BIGINT      NOT NULL,
    max_runs       BIGINT,
    enabled        BOOLEAN     NOT NULL DEFAULT TRUE,
    created_at     TIMESTAMPTZ NOT NULL,
    updated_at     TIMESTAMPTZ NOT NULL
);

CREATE INDEX schedules_enabled_idx ON schedules (enabled);

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
    -- zstd-JSON fallback blob, used strictly when the parquet write fails.
    result_data    BYTEA,
    -- Path to the parquet result file, relative to the data dir. Retention,
    -- `from saved`, and result download all key off it.
    result_path    TEXT
);

CREATE INDEX report_runs_schedule_started_idx
    ON report_runs (schedule_id, started_at DESC, id DESC);

CREATE INDEX report_runs_saved_query_idx
    ON report_runs (saved_query_id, started_at DESC, id DESC);

-- The no-concurrent-run guard: at most one 'running' row per schedule.
-- start_run INSERTs directly and maps a 23505 on this index to "already
-- running" (sqlite's process-mutex atomicity, redesigned for pg).
CREATE UNIQUE INDEX report_runs_one_running
    ON report_runs (schedule_id) WHERE status = 'running';
