-- The scheduler tick's due listing (ADR-0018 amended 2026-11-11): only
-- schedules that may still start a run. A disabled or spent schedule leaves
-- the index, so the tick never reads it; raising or removing the cap or
-- enabling the schedule brings it back. The claim re-checks the budget under
-- its row locks, so this predicate selects candidates and authorizes nothing.
-- The listing seeks (next_fire_at, id) in order, with no sort.

CREATE INDEX schedules_due_idx ON schedules (next_fire_at, id)
    WHERE enabled AND (max_runs IS NULL OR runs_claimed < max_runs);
