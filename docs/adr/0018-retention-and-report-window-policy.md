# Retention and report windows: age is per-env policy, pressure is survival, the schedule owns its window

status: accepted (2026-08-18) — prep ruling record for #97

Two parked halves of PR #58, one subject: who owns time. Retention is one
install-wide `max_age_days`, so keeping `prod` a year forces keeping the
`lab` firehose a year. Scheduled reports execute the saved DSL verbatim, so
a nightly schedule over `last=2h` covers 2 of every 24 hours, fire-time
drift silently shifts coverage, and a run's stored query text reproduces
nothing.

## Retention rulings

1. **Age is per-env policy; pressure is install-wide survival.**
   `[retention.env.<name>] max_age_days` overrides the global default
   (absent = inherit; `0` = no age sweep for that env). There is no per-env
   disk floor and no per-env byte quota — one filesystem does not have
   separable free space.
2. **Pressure deletes by expiry ratio** — `age / that env's effective
   max_age_days`, descending, ties by older date — so per-env intent
   survives the emergency sweep. Global oldest-first is rejected: under
   `prod=365 lab=7` it eats 300-day prod evidence while 6-day lab noise
   survives.
3. **Nothing is exempt from pressure.** A keep-forever env ranks last but
   stays eligible; a sweep that cannot reclaim is a wedged daemon. Age
   remains a maximum, never a guaranteed minimum.
4. Per-env rules sit **inside** the existing gates (repin marker/staging
   suppression, per-deletion re-read, set-aside) and can never license a
   deletion those gates deny. `scheduled/` stays governed solely by
   `report_retention_days`.
5. A config entry naming an env with no directory warns at boot, not
   fatal; envs on disk with no entry inherit the global, never a sibling.

## Report-window rulings

6. **The window is a property of the schedule, not the query text.**
   Schedule modes: absent (legacy — DSL executed verbatim),
   `window = "since_last"` (tiling `[previous window_end, fire - lag)`),
   `window = "<duration>"` (fixed trailing). `lag` (default 0) shifts both
   bounds back to cover stragglers.
7. **Coexistence is refused, both directions, at write time**: a schedule
   window against a query carrying `last=`/`earliest=`/`latest=` is a 400
   naming both, and so is editing such a time clause into a windowed saved
   query. Two spellings of one interval never coexist.
8. **The axis is `_time`** (sender truth — reports agree with interactive
   queries of the same interval; the layout prunes by it), with `lag` as
   the late-arrival allowance. `_ingested` windows (exactly-once coverage
   at the cost of disagreeing with every interactive query and defeating
   partition pruning) are rejected for this design.
9. **The watermark advances only on success**; a failed/timed-out run
   leaves it, so the next success covers the gap. Missed runs **coalesce
   into one window, never backfill** N runs. A gap beyond
   `max_catchup_intervals` (unit: intervals, default 24) **clamps forward
   and flags** — `window_truncated` on the run row plus a metric — never
   silently, and never wedging the schedule.
10. **Windows are half-open `[start, end)`, and `latest=` becomes
    exclusive DSL-wide** — the only interval shape that composes; tiled
    windows cannot double-count a boundary event. Breaking language
    change, priced at zero by standing rule.
11. The scheduler materializes the window as `earliest`/`latest` on the
    executed DSL and `report_runs.query` stores the **resolved** text — a
    run is reproducible by paste. `report_runs` gains nullable
    `window_start`/`window_end`/`window_truncated`; legacy/query-owned
    runs report NULL, never backfilled.
12. **`from saved` never receives a window** (its input is stored results,
    not ingest events): a `from saved` query is refused a schedule window,
    and a producing run's bounds surface as run **metadata**, never as
    injected columns. Interactive execution of a scheduled saved query
    carries no window — the text is what the human typed.
13. **Every successful run is recorded, zero-row runs included** — the run
    row carries its window even when no result file exists (today's
    empty-result path writes no parquet; that gap is fixed alongside).
14. The first `since_last` run with no predecessor covers
    `[fire - lag - interval, fire - lag)`. Editing the saved DSL does not
    reset the watermark; the per-run resolved snapshot is the audit trail.

## Slicing

Two implementation issues: the schedule window (ships first — today's
behavior is a wrong answer, not merely coarse; includes the `latest=`
exclusivity change as prerequisite), then per-env age retention with the
ratio comparator (a delete path — its own review posture). Web-UI window
display and catch-up dashboards ride later work.
