-- Force ceilings on a repin job, and the operator's acknowledgement of a
-- degraded pin (issue #111).
--
-- Two unrelated-looking halves, one theme: both let an operator say "I have
-- seen this, and here is what I accept" in a form the server can check later
-- rather than a flag it can only trust.
--
-- HALF ONE: `--force` was a blank check. The operator read a plan, accepted
-- its losses, and the shadow build then ran for minutes against a corpus
-- ingest keeps growing — whatever the finished rewrite turned out to have
-- lost, force covered it. The four columns record the numbers instead: what
-- the request ASKED for (nullable, absent when the operator stated nothing)
-- and what the job was HELD to (resolved once at plan time from this job's
-- own scan). A job row written before this migration reads NULL on all four,
-- which `repin::ceiling::ForceTerms` reads as the legacy blank check: there
-- is no honest number to hold it to, so it keeps the old behaviour.
--
-- All four are BIGINT because postgres has no unsigned integer; the request
-- surface refuses a value above i64::MAX rather than storing a wrapped one.
-- Zero is a legitimate ceiling ("force the ambiguity, but not one row of
-- loss"), so the CHECKs are `>= 0`, not `> 0`.
ALTER TABLE repin_jobs ADD COLUMN max_nulled_rows BIGINT;
ALTER TABLE repin_jobs ADD CONSTRAINT repin_jobs_max_nulled_rows_check
    CHECK (max_nulled_rows >= 0);
ALTER TABLE repin_jobs ADD COLUMN max_ambiguous_rows BIGINT;
ALTER TABLE repin_jobs ADD CONSTRAINT repin_jobs_max_ambiguous_rows_check
    CHECK (max_ambiguous_rows >= 0);
ALTER TABLE repin_jobs ADD COLUMN accepted_max_nulled_rows BIGINT;
ALTER TABLE repin_jobs ADD CONSTRAINT repin_jobs_accepted_max_nulled_rows_check
    CHECK (accepted_max_nulled_rows >= 0);
ALTER TABLE repin_jobs ADD COLUMN accepted_max_ambiguous_rows BIGINT;
ALTER TABLE repin_jobs ADD CONSTRAINT repin_jobs_accepted_max_ambiguous_rows_check
    CHECK (accepted_max_ambiguous_rows >= 0);

-- HALF TWO: the degraded badge an operator has decided not to act on.
--
-- The analyzer's verdict is read-time and span-based, so a pin an operator
-- has judged acceptable ("that sender really does send `n/a`, and `_raw`
-- keeps it") badges forever, and a badge nobody can dismiss is a badge
-- nobody reads. An ack suppresses the verdict up to the evidence the
-- operator saw, and no further.
--
-- `evidence_through` is the acknowledged EPISODE HIGH-WATER: the
-- `sum(field_conflict_stats.episodes)` for the field at the instant of the
-- ack. It is NOT a timestamp, and that is the whole point. An ack keyed on
-- `last_at` would suppress every conflict recorded in the same clock tick as
-- the read, and compaction writes many episodes per second under load, so a
-- tie at microsecond resolution silently acknowledges evidence the operator
-- never saw. An episode count is monotonic and counts only what has actually
-- been recorded: suppression is `episodes <= evidence_through`, so the very
-- next episode re-raises the badge with nothing to guess about.
--
-- The FK CASCADEs: an ack indicts a pin, so it cannot outlive one. A repin
-- deletes the row explicitly inside the cutover transaction
-- (`RepinStore::finish_cutover`) — the cascade is the backstop for a pin
-- that disappears some other way, not the mechanism.
--
-- `acked_by` is the verified key's stable prefix rather than its display
-- name: this row is long-lived, and a renamed or rotated key must not
-- rewrite who acknowledged what.
--
-- Every constraint is NAMED, per the 0001 convention.
CREATE TABLE field_degraded_ack (
    field            TEXT
        CONSTRAINT field_degraded_ack_pkey PRIMARY KEY
        CONSTRAINT field_degraded_ack_field_fkey
            REFERENCES field_types (field) ON DELETE CASCADE,
    acked_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    acked_by         TEXT        NOT NULL,
    -- Operator prose, capped in BYTES: the column is displayed, never
    -- parsed, and 1 KiB is a sentence of context, not a document.
    note             TEXT
        CONSTRAINT field_degraded_ack_note_length_check
        CHECK (octet_length(note) <= 1024),
    evidence_through BIGINT      NOT NULL
        CONSTRAINT field_degraded_ack_evidence_through_check
        CHECK (evidence_through >= 0)
);
