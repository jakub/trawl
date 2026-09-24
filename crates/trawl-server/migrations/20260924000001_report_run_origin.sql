-- Whether a report run was claimed by the scheduler or fired by hand
-- (ADR-0018 amended 2026-09-23). Every claim writes it. NULL is a run
-- claimed before this column existed and is never backfilled: nothing
-- recorded how those runs started.

ALTER TABLE report_runs
    ADD COLUMN origin TEXT,
    ADD CONSTRAINT report_runs_origin
    CHECK (origin IS NULL OR origin IN ('scheduled', 'manual'));
