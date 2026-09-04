-- Scheduler-owned report windows (ADR-0018 rulings 6-14, issue #107): the
-- window a scheduled report covers becomes a property of the SCHEDULE, and
-- each run records the interval it actually covered.
--
-- `schedules` gains four window columns plus the planned fire cursor:
--   window_kind/window_secs  the mode. NULL = legacy: the saved DSL is
--                            executed verbatim and trawl owns no window.
--                            'since_last' tiles forward from the previous
--                            successful run and carries no span;
--                            'fixed' is a trailing span and must carry one.
--   lag_secs                 straggler allowance, shifting both bounds back.
--                            0 is the default and a legal value, so the
--                            column is NOT NULL rather than nullable.
--   covered_through          the 'since_last' watermark: the end of the
--                            newest window a SUCCESSFUL run covered. Only
--                            `finish_run` advances it, and only on success,
--                            so a failed run leaves the gap for the next one.
--   next_fire_at             the planned cursor. The scheduler fires on this
--                            instant rather than on `last_run + interval`,
--                            which is what stops fire-time drift from
--                            shifting coverage tick after tick.
--
-- `next_fire_at` is backfilled per schedule from its newest run before the
-- NOT NULL is set: a plain `DEFAULT now()` would make every standing
-- schedule due at the same instant on the first tick after upgrade, firing
-- the whole install at once. A schedule with no runs yet has nothing to
-- extrapolate from and takes `now()`, which is when it would have become
-- due anyway.
--
-- `report_runs` gains the two bounds, the truncation flag and the MODE the
-- run was claimed under, all nullable and never backfilled (ruling 11): a
-- legacy run has no window, and inventing one would claim coverage nothing
-- proves. `window_truncated` is NULL for the same reason FALSE is wrong
-- there — FALSE means "windowed and complete".
--
-- The run carries its own `window_kind` because the schedule's can be
-- edited while the run is in flight. `finish_run` advances the watermark
-- for a `since_last` run, and asking the SCHEDULE at finish time makes the
-- answer depend on whether the operator's edit committed before or after
-- the run finished. The mode a run was CLAIMED under is a fact about that
-- run; the schedule's current mode is not.
--
-- Every constraint is NAMED, per the 0001 convention.

ALTER TABLE schedules
    ADD COLUMN window_kind     TEXT,
    ADD COLUMN window_secs     BIGINT,
    ADD COLUMN lag_secs        BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN covered_through TIMESTAMPTZ,
    ADD COLUMN next_fire_at    TIMESTAMPTZ;

-- Backfill the planned cursor so existing schedules keep their cadence
-- instead of all firing at once.
UPDATE schedules s SET next_fire_at = COALESCE(
    (SELECT r.started_at + s.interval_secs * INTERVAL '1 second' FROM report_runs r
      WHERE r.schedule_id = s.id ORDER BY r.started_at DESC, r.id DESC LIMIT 1),
    now());

ALTER TABLE schedules ALTER COLUMN next_fire_at SET NOT NULL;

ALTER TABLE schedules
    ADD CONSTRAINT schedules_window_kind CHECK (
        window_kind IS NULL OR window_kind IN ('since_last', 'fixed')),
    ADD CONSTRAINT schedules_window_shape CHECK (
         (window_kind IS NULL AND window_secs IS NULL)
      OR (window_kind = 'since_last' AND window_secs IS NULL)
      OR (window_kind = 'fixed' AND window_secs IS NOT NULL AND window_secs >= 60)),
    ADD CONSTRAINT schedules_lag_nonneg CHECK (lag_secs >= 0);

ALTER TABLE report_runs
    ADD COLUMN window_start     TIMESTAMPTZ,
    ADD COLUMN window_end       TIMESTAMPTZ,
    ADD COLUMN window_truncated BOOLEAN,
    ADD COLUMN window_kind      TEXT;

-- The four columns are one fact: a run either carries a half-open window
-- claimed under a named mode, or carries none. A partial bound would be a
-- window nothing can read.
ALTER TABLE report_runs
    ADD CONSTRAINT report_runs_window_kind CHECK (
        window_kind IS NULL OR window_kind IN ('since_last', 'fixed')),
    ADD CONSTRAINT report_runs_window_shape CHECK (
        CASE WHEN window_start IS NULL
             THEN window_end IS NULL AND window_truncated IS NULL AND window_kind IS NULL
             ELSE window_end IS NOT NULL AND window_truncated IS NOT NULL
                  AND window_kind IS NOT NULL AND window_start < window_end
        END);
