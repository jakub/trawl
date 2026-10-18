-- The row id of the key that requested a repin job (ADR-0054 amended
-- 2026-10-17). A status read by id with `schema_write` alone answers only
-- the jobs the calling key requested, and it matches on this id, never on
-- the display name in `requested_by`, which an operator chooses and two
-- keys may share. NULL is a job no key requested, which that read answers
-- for nobody.

ALTER TABLE repin_jobs
    ADD COLUMN requested_by_key_id BIGINT;
