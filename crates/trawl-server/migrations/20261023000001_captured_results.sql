-- Captured results (ADR-0057): one row per immutable capture kept under the
-- results root. The id is a random 128-bit value, written as 32 lowercase hex
-- digits, and it is also the file's name, so its shape is checked here.
--
-- key_id is the fleet key that made the capture, carried by value as in every
-- other table: the keys live in the fleet database. A temporary capture has a
-- deadline in expires_at; a kept one (attached to an investigation, ADR-0059)
-- has none. gone_at is the instant the result stopped being readable: its
-- deadline for an expiry, the prune time for a prune. Tombstones are deleted
-- 24 hours after gone_at, so the owner can tell expired from never-existed.
--
-- A live source has resolved bounds; a stored source (`from saved`) has none.
-- columns and column_facts are the trawl-api JSON of Vec<ResultColumn> and
-- Vec<ColumnFacts>, positional with each other, kept as TEXT because JSONB
-- refuses \u0000, which a column name or a listed value may hold. A capture
-- with no columns has no file: file_bytes is 0 and digest (blake3, 32 bytes)
-- is NULL. file_missing records a committed row whose file was found absent.
-- investigation_id has no foreign key until the investigations table exists.
--
-- Every lifecycle statement binds the server's clock as a parameter; none
-- reads now(). Constraint names are part of the store error contract.

CREATE TABLE captured_results (
    id               TEXT        NOT NULL,
    key_id           BIGINT      NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL,
    expires_at       TIMESTAMPTZ,
    gone_at          TIMESTAMPTZ,
    state            TEXT        NOT NULL
        CONSTRAINT captured_results_state_check
        CHECK (state IN ('temporary', 'kept', 'expired', 'pruned')),
    query            TEXT        NOT NULL,
    executed_query   TEXT        NOT NULL,
    source           TEXT        NOT NULL
        CONSTRAINT captured_results_source_check
        CHECK (source IN ('live', 'stored')),
    shape            TEXT        NOT NULL
        CONSTRAINT captured_results_shape_check
        CHECK (shape IN ('events', 'whole')),
    earliest         TIMESTAMPTZ,
    latest           TIMESTAMPTZ,
    columns          TEXT        NOT NULL,
    column_facts     TEXT        NOT NULL,
    captured_rows    BIGINT      NOT NULL,
    total_rows       BIGINT      NOT NULL,
    wire_bytes       BIGINT      NOT NULL,
    file_bytes       BIGINT      NOT NULL,
    digest           BYTEA,
    file_missing     BOOLEAN     NOT NULL DEFAULT FALSE,
    format_version   SMALLINT    NOT NULL,
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
    CONSTRAINT captured_results_rows CHECK (
        captured_rows >= 0 AND captured_rows <= total_rows),
    CONSTRAINT captured_results_sizes CHECK (wire_bytes >= 0 AND file_bytes >= 0),
    CONSTRAINT captured_results_digest_iff_file CHECK ((digest IS NULL) = (file_bytes = 0)),
    CONSTRAINT captured_results_digest_len CHECK (digest IS NULL OR octet_length(digest) = 32),
    CONSTRAINT captured_results_format_version CHECK (format_version > 0)
);

-- A key's live temporary captures, for its list and its quota.
CREATE INDEX captured_results_key_live ON captured_results (key_id, expires_at)
    WHERE state = 'temporary';
-- The expiry sweep.
CREATE INDEX captured_results_sweep ON captured_results (state, expires_at);
-- The tombstone sweep.
CREATE INDEX captured_results_gone ON captured_results (gone_at)
    WHERE gone_at IS NOT NULL;
