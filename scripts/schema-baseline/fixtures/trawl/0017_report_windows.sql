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

-- Clamp legacy intervals into the domain the duration grammar now enforces.
--
-- `parse_duration_secs` caps every duration at MAX_DURATION_SECS, 315360000
-- seconds (ten years), because each one becomes date arithmetic somewhere.
-- Nothing bounded interval_secs above 60 before that cap existed, and a row
-- above it is not merely odd. `plan_due_run` cannot represent
-- `next_fire_at + interval`, so it returns PlanError::Arithmetic: the
-- schedule is due, nothing advances, and every poll spends a failed
-- transaction on it forever. Clamping is the repair that leaves the
-- schedule runnable, and a ten-year cadence is what a nonsense interval
-- already meant. The backfill below also depends on it: `interval *
-- INTERVAL '1 second'` overflows postgres' interval type well short of
-- BIGINT, and one absurd row would abort the whole migration and leave the
-- daemon unable to boot.
UPDATE schedules SET interval_secs = 315360000 WHERE interval_secs > 315360000;

-- Backfill the planned cursor so existing schedules keep their cadence
-- instead of all firing at once. A schedule with no runs has nothing to
-- extrapolate from and takes now(), which is when it would have become due
-- anyway.
UPDATE schedules s SET next_fire_at = COALESCE(
    (SELECT r.started_at + s.interval_secs * INTERVAL '1 second'
       FROM report_runs r
      WHERE r.schedule_id = s.id ORDER BY r.started_at DESC, r.id DESC LIMIT 1),
    now());

ALTER TABLE schedules ALTER COLUMN next_fire_at SET NOT NULL;

ALTER TABLE schedules
    ADD CONSTRAINT schedules_window_kind CHECK (
        window_kind IS NULL OR window_kind IN ('since_last', 'fixed')),
    -- Written as a CASE so every branch is TRUE or FALSE, never NULL. A
    -- CHECK accepts NULL, so the obvious OR-of-arms spelling has a hole:
    -- for (window_kind NULL, window_secs 60) the fixed arm evaluates
    -- NULL = 'fixed' AND TRUE = NULL, the other arms are FALSE, and
    -- FALSE OR FALSE OR NULL is NULL. The row would be stored with a span
    -- and no mode, which no decoder can read back.
    ADD CONSTRAINT schedules_window_shape CHECK (
        CASE WHEN window_kind IS NULL         THEN window_secs IS NULL
             WHEN window_kind = 'since_last'  THEN window_secs IS NULL
             WHEN window_kind = 'fixed'       THEN window_secs IS NOT NULL
                                                   AND window_secs >= 60
             ELSE FALSE
        END),
    ADD CONSTRAINT schedules_lag_nonneg CHECK (lag_secs >= 0),
    -- The database agrees with MAX_DURATION_SECS rather than trusting the
    -- grammar to be the only writer. All three columns are seconds that
    -- end up in date arithmetic — a fire cursor, a window bound, a lag
    -- applied to both — and the planner has to be able to represent every
    -- one of them. `migration_0017_spells_the_same_duration_cap` in
    -- store/schedule.rs reads this file and fails if the literal drifts
    -- from the constant.
    ADD CONSTRAINT schedules_interval_within_cap CHECK (interval_secs <= 315360000),
    ADD CONSTRAINT schedules_window_secs_within_cap CHECK (
        window_secs IS NULL OR window_secs <= 315360000),
    ADD CONSTRAINT schedules_lag_within_cap CHECK (lag_secs <= 315360000);

ALTER TABLE report_runs
    ADD COLUMN window_start     TIMESTAMPTZ,
    ADD COLUMN window_end       TIMESTAMPTZ,
    ADD COLUMN window_truncated BOOLEAN,
    ADD COLUMN window_kind      TEXT;

-- The four columns are one fact: a run either carries a half-open window
-- claimed under a named mode, or carries none. A partial bound would be a
-- window nothing can read.
--
-- A CASE here for the same reason as `schedules_window_shape`: every branch
-- has to be TRUE or FALSE. `window_start < window_end` is the one predicate
-- that can be NULL, so it sits last in the ELSE branch, behind the three
-- IS NOT NULL tests. A FALSE from any of those makes the AND chain FALSE
-- whatever follows, so the comparison is only reached with two real bounds.
ALTER TABLE report_runs
    ADD CONSTRAINT report_runs_window_kind CHECK (
        window_kind IS NULL OR window_kind IN ('since_last', 'fixed')),
    ADD CONSTRAINT report_runs_window_shape CHECK (
        CASE WHEN window_start IS NULL
             THEN window_end IS NULL AND window_truncated IS NULL AND window_kind IS NULL
             ELSE window_end IS NOT NULL AND window_truncated IS NOT NULL
                  AND window_kind IS NOT NULL AND window_start < window_end
        END);
