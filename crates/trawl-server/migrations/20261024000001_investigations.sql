-- Investigations (ADR-0059): trawl's first installation-owned object. No
-- column scopes a row to a key: `investigation_read` reads every
-- investigation and `investigation_write` writes to any of them, so the
-- key ids here are attribution, each beside a snapshot of the key's prefix
-- and name that still read after the key is revoked or renamed. Key ids are
-- carried by value, as in every other table: the keys live in the fleet
-- database. They stay server side (ownership and the replay check) and are
-- never serialized: the wire attributes by prefix, the key's stable
-- fingerprint, because fleet is shared across applications and its
-- monotonic ids would give every reader a cross-surface correlation
-- handle. A prefix is 8 characters of [A-Za-z0-9_-] (fleet-auth
-- api_keys_prefix_format), so at most 8 bytes.
--
-- The id is a random 128-bit value the MCP server mints, written as 32
-- lowercase hex digits, the captured result id's shape. seq is the
-- sequence of the latest write: creation is 0, and every later write
-- advances it by one. creation is the CreateInvestigation request as the
-- store encoded it, kept apart from the mutable title, question and scope
-- and never updated, so a replayed create compares against what was first
-- asked. The text bounds are the trawl-api constants: title 200 bytes,
-- question and scope 2048 each; creation holds those three JSON-escaped
-- (at most six bytes per input byte) and is bounded at 32 KiB.
--
-- Evidence retention has two clocks (amends ADR-0018). retain_until slides
-- to the latest change plus the idle period, never past retain_ceiling;
-- retain_ceiling starts at creation plus the ceiling period and only an
-- extension by server_manage raises it. Kept captures whose investigation
-- passed retain_until are pruned; the investigation's own text stays until
-- an operator deletes it.

CREATE TABLE investigations (
    id                TEXT        NOT NULL,
    title             TEXT        NOT NULL,
    question          TEXT        NOT NULL,
    scope             TEXT        NOT NULL,
    status            TEXT        NOT NULL
        CONSTRAINT investigations_status_check
        CHECK (status IN ('open', 'closed')),
    seq               BIGINT      NOT NULL,
    created_at        TIMESTAMPTZ NOT NULL,
    updated_at        TIMESTAMPTZ NOT NULL,
    retain_until      TIMESTAMPTZ NOT NULL,
    retain_ceiling    TIMESTAMPTZ NOT NULL,
    created_by_key_id BIGINT      NOT NULL,
    created_by_prefix TEXT        NOT NULL,
    created_by_name   TEXT        NOT NULL,
    updated_by_key_id BIGINT      NOT NULL,
    updated_by_prefix TEXT        NOT NULL,
    updated_by_name   TEXT        NOT NULL,
    creation          TEXT        NOT NULL,
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

-- The list: open first ('open' sorts after 'closed'), then the most
-- recently changed.
CREATE INDEX investigations_listing ON investigations (status DESC, updated_at DESC);

-- One row per log entry. seq is the investigation sequence that created the
-- entry and updated_seq the one that last changed it: seq itself, or later
-- for a step whose outcome was written after it was appended. A reader asks
-- for the entries with updated_seq past the one it saw.
--
-- payload is the trawl-api JSON of EntryPayload, tagged with the same kind
-- as the kind column, kept as TEXT because JSONB refuses \u0000. Its bound
-- is sized for the largest entry, a step: the executed text is at most the
-- parser's 64 KiB input limit (MAX_QUERY_LEN) plus the spliced window and
-- imposed order, JSON escaping grows a byte to at most six (384 KiB), the
-- purpose adds at most 6 KiB, and the request id, bounds and outcome a few
-- hundred bytes more. 512 KiB leaves headroom over that; every other kind
-- is under 64 KiB escaped.
--
-- outcome, outcome_code and result_id repeat a step's outcome as columns so
-- the boot pass finds running steps (outcome IS NULL) and completion is
-- one-shot without parsing the payload. Other kinds leave them NULL.

CREATE TABLE investigation_log (
    investigation_id TEXT        NOT NULL,
    seq              BIGINT      NOT NULL,
    updated_seq      BIGINT      NOT NULL,
    kind             TEXT        NOT NULL
        CONSTRAINT investigation_log_kind_check
        CHECK (kind IN ('step', 'keep', 'finding', 'finding_status', 'note', 'status',
                        'title', 'question')),
    actor_key_id     BIGINT      NOT NULL,
    actor_prefix     TEXT        NOT NULL,
    actor_name       TEXT        NOT NULL,
    at               TIMESTAMPTZ NOT NULL,
    payload          TEXT        NOT NULL,
    outcome          TEXT
        CONSTRAINT investigation_log_outcome_check
        CHECK (outcome IN ('success', 'refused', 'timeout', 'failed', 'aborted',
                           'interrupted')),
    outcome_code     TEXT,
    result_id        TEXT,
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

-- A read since a sequence.
CREATE INDEX investigation_log_updated ON investigation_log (investigation_id, updated_seq);
-- The boot pass over steps whose work never recorded an outcome.
CREATE INDEX investigation_log_running ON investigation_log (investigation_id, seq)
    WHERE kind = 'step' AND outcome IS NULL;

-- captured_results.investigation_id was created as a BIGINT placeholder
-- before investigations existed, and nothing wrote it. Retype it to the id
-- it references, refusing to run if any row holds a value, since no BIGINT
-- names an investigation. Deleting an investigation clears it on the rows
-- it held; the delete prunes those rows first, so none is left kept.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM captured_results WHERE investigation_id IS NOT NULL) THEN
        RAISE EXCEPTION 'captured_results.investigation_id holds values; it must be empty before it is retyped';
    END IF;
END
$$;

ALTER TABLE captured_results
    ALTER COLUMN investigation_id TYPE TEXT USING NULL,
    ADD CONSTRAINT captured_results_investigation_fk FOREIGN KEY (investigation_id)
        REFERENCES investigations (id) ON DELETE SET NULL;

-- An investigation's evidence: its usage and the retention prune.
CREATE INDEX captured_results_investigation ON captured_results (investigation_id, state)
    WHERE investigation_id IS NOT NULL;
