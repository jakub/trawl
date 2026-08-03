-- Field catalog (ADR-0009 slice 2): write-time type authority for every
-- dynamic field. Owned and boot-migrated by trawld like 0001 — sole writer
-- enforced by the session advisory lock taken before migration.
--
-- Every constraint is NAMED, per the 0001 convention.

-- The pins. Key is the field name ALONE — global, not per-service:
-- per-service typing would be locally correct and break cross-service
-- queries, which are the ones worth having.
--
-- Bounded at store::catalog::MAX_PINNED_FIELDS rows, and a row here is
-- permanent (add-only until the repin rewrite, #53) — so the ingest path
-- may claim at most half the FREE rows per batch, keeping the table out of
-- reach of a single burst. Never DELETE from this table by hand: a standing
-- parquet file carrying the column would then be outside the write-time
-- conformance invariant, which is what makes union_by_name safe.
CREATE TABLE field_types (
    field       TEXT
        CONSTRAINT field_types_pkey PRIMARY KEY,
    duckdb_type TEXT        NOT NULL
        CONSTRAINT field_types_type_check
        CHECK (duckdb_type IN ('BOOLEAN', 'BIGINT', 'DOUBLE', 'TIMESTAMP', 'VARCHAR')),
    pinned_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Which service's batch set the pin ('_declared' for the seed below).
    pinned_from TEXT
);

-- Per-service field observations, powering schema browse (#51).
-- Rows are EVER-OBSERVED: first_seen/last_seen are maintained by
-- compaction and retention never reconciles them — consumers must window
-- on last_seen so aged-out fields are not offered as live.
--
-- Bounded on BOTH client-chosen axes: field by the pin cap
-- (store::catalog::MAX_PINNED_FIELDS — an unpinned field is never
-- observed), service by a rolling per-field window of the
-- most-recently-seen store::catalog::MAX_SERVICES_PER_FIELD, trimmed in the
-- same transaction as the upsert. Nothing else ever reclaims a row here
-- (unlike the parallel per-service parquet/WAL axis, which retention
-- reclaims), so without that window a shipper cycling service names would
-- grow this table for the life of the install. Eviction is
-- least-recently-seen, so a live service — refreshed by every batch it
-- compacts — is never evicted by a cycling one.
CREATE TABLE field_services (
    field      TEXT        NOT NULL,
    service    TEXT        NOT NULL,
    first_seen TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen  TIMESTAMPTZ NOT NULL DEFAULT now(),
    row_count  BIGINT      NOT NULL DEFAULT 0,

    CONSTRAINT field_services_pkey PRIMARY KEY (field, service)
);

-- Conflict evidence, powering the schema-health dashboard. APPEND-ONLY:
-- one row per LOSSY conforming cast per batch (rows_nulled > 0), never
-- aggregated. A cast that nulls nothing is not recorded: it would append a
-- row per (field, service) on every compaction tick forever for a sender
-- that is losing nothing. What a genuinely lossy sender appends is bounded
-- by a rolling per-field window (store::catalog::MAX_CONFLICTS_PER_FIELD),
-- trimmed in the same transaction as the insert — the table is evidence,
-- not a ledger, and its cardinality is client-chosen.
CREATE TABLE field_conflicts (
    id            BIGSERIAL
        CONSTRAINT field_conflicts_pkey PRIMARY KEY,
    field         TEXT        NOT NULL,
    service       TEXT        NOT NULL,
    observed_type TEXT        NOT NULL,
    expected_type TEXT        NOT NULL,
    rows_nulled   BIGINT      NOT NULL,
    at            TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX field_conflicts_field_at_idx
    ON field_conflicts (field, at DESC);

-- Catalog identity + boot-conformance bookkeeping. Exactly one row; the
-- catalog_id is mirrored into the data root's CATALOG marker file so a
-- repointed DATABASE_URL or a restored-from-backup data root is
-- self-detecting (identity mismatch forces the conformance pass to re-run).
CREATE TABLE catalog_state (
    singleton    BOOLEAN     NOT NULL DEFAULT TRUE
        CONSTRAINT catalog_state_pkey PRIMARY KEY
        CONSTRAINT catalog_state_singleton CHECK (singleton),
    catalog_id   UUID        NOT NULL DEFAULT gen_random_uuid(),
    conformed_at TIMESTAMPTZ
);

INSERT INTO catalog_state (singleton) VALUES (TRUE);

-- Pre-seed the declared envelope (ADR-0009): the declared schema is the
-- catalog's first citizen, not an inference. MUST mirror
-- trawl_core::schema::ENVELOPE_TYPES exactly (parity-tested).
INSERT INTO field_types (field, duckdb_type, pinned_from) VALUES
    ('_time',         'TIMESTAMP', '_declared'),
    ('_ingested',     'TIMESTAMP', '_declared'),
    ('_raw',          'VARCHAR',   '_declared'),
    ('_repairs',      'VARCHAR',   '_declared'),
    ('env',           'VARCHAR',   '_declared'),
    ('service',       'VARCHAR',   '_declared'),
    ('host',          'VARCHAR',   '_declared'),
    ('severity',      'BIGINT',    '_declared'),
    ('severity_text', 'VARCHAR',   '_declared'),
    ('message',       'VARCHAR',   '_declared');
