-- Repin jobs (ADR-0011 slice B, issue #53): the persisted, one-at-a-time
-- operator-triggered field repin. One row per job, dry runs included — the
-- job row IS the dry-run report, and the scan/refusal/execution lifecycle
-- is one code path.
--
-- Every constraint is NAMED, per the 0001 convention.

CREATE TABLE repin_jobs (
    id            BIGSERIAL
        CONSTRAINT repin_jobs_pkey PRIMARY KEY,
    field         TEXT        NOT NULL,
    from_type     TEXT        NOT NULL
        CONSTRAINT repin_jobs_from_type_check
        CHECK (from_type IN ('BOOLEAN', 'BIGINT', 'DOUBLE', 'TIMESTAMP', 'VARCHAR')),
    to_type       TEXT        NOT NULL
        CONSTRAINT repin_jobs_to_type_check
        CHECK (to_type IN ('BOOLEAN', 'BIGINT', 'DOUBLE', 'TIMESTAMP', 'VARCHAR')),
    dry_run       BOOLEAN     NOT NULL,
    force         BOOLEAN     NOT NULL DEFAULT FALSE,
    -- Closed status vocabulary:
    --   running             — claimed; scanning, building, or cutting over
    --   succeeded           — terminal: dry-run report ready / corpus repinned
    --   failed              — terminal: error (recorded), corpus untouched or
    --                         recovered to the pre-repin generation
    --   refused_needs_force — terminal: the scan projected nulled values and
    --                         the request carried no force flag (the 409 body
    --                         carries the plan)
    --   blocked             — terminal: the cutover could not drain queries
    --                         within its budget; shadow retained for a retry
    status        TEXT        NOT NULL DEFAULT 'running'
        CONSTRAINT repin_jobs_status_check
        CHECK (status IN ('running', 'succeeded', 'failed',
                          'refused_needs_force', 'blocked')),
    requested_by  TEXT,
    started_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at   TIMESTAMPTZ,
    error         TEXT,

    -- The dry-run/scan plan (also stamped on executing jobs — the scan is
    -- one code path).
    files_total     BIGINT NOT NULL DEFAULT 0,
    rows_carrying   BIGINT NOT NULL DEFAULT 0,
    projected_nulls BIGINT NOT NULL DEFAULT 0,
    resurrectable   BIGINT NOT NULL DEFAULT 0,
    affected_bytes  BIGINT NOT NULL DEFAULT 0,

    -- Rewrite progress/outcome.
    files_done       BIGINT NOT NULL DEFAULT 0,
    rows_rewritten   BIGINT NOT NULL DEFAULT 0,
    rows_nulled      BIGINT NOT NULL DEFAULT 0,
    rows_resurrected BIGINT NOT NULL DEFAULT 0
);

-- One repin at a time, install-wide: claim INSERTs directly and maps a
-- 23505 on this index to StoreError::RepinAlreadyRunning (the
-- report_runs_one_running pattern, global rather than per-schedule).
CREATE UNIQUE INDEX repin_jobs_one_running
    ON repin_jobs ((TRUE)) WHERE status = 'running';

-- The status surface reads "running first, else newest".
CREATE INDEX repin_jobs_started_idx
    ON repin_jobs (started_at DESC, id DESC);
