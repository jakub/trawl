-- The runs a schedule has claimed over its life, its spent run budget
-- (ADR-0018 amended 2026-11-06, "max_runs is a lifetime budget"). Every
-- claim increments it in the claim's own transaction while max_runs is
-- absent or runs_claimed < max_runs; retention and run failures never
-- lower it.
--
-- The seed counts the runs each schedule still has, so it is a lower bound:
-- runs retention already deleted cannot be counted. A schedule whose older
-- runs are gone may claim up to its cap again before it stops.

ALTER TABLE schedules
    ADD COLUMN runs_claimed BIGINT NOT NULL DEFAULT 0
        CONSTRAINT schedules_runs_claimed_nonneg CHECK (runs_claimed >= 0);

UPDATE schedules s
SET runs_claimed = kept.runs
FROM (SELECT schedule_id, COUNT(*) AS runs FROM report_runs GROUP BY schedule_id) kept
WHERE kept.schedule_id = s.id;
