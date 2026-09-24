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

*Amended 2026-09-12 (UI-audit remainder prep): the web form is the third
write door beside the API and the TUI. It renders the window as an
exclusive three-way choice (query text, since last run, fixed span), shows
lag only beside a window, and sends exactly what it shows: query mode
sends neither field, a blank lag sends none. The browser validates nothing
the server already validates; the envelope message is rendered under Save
with the draft intact. Switching a windowed schedule to query mode is an
edit plus Save with an inline hint, not a confirmation: the server keeps
the watermark, so the change is reversible. The manual run control is
absent for a saved windowed schedule (ADR-0025). Catch-up dashboards still
ride later work.*

*Amended 2026-09-23 (#236 prep): a windowed schedule can be run by hand,
superseding "the manual run control is absent for a saved windowed
schedule" above. A manual run is the schedule's next window fired early,
not a side run beside it: a side run overlaps the next scheduled window
and every `from saved` reader counts those events twice. The server reads
the clock once it holds the claim locks; call it `t`. `since_last` covers
`[covered_through, t - lag)` under the same catch-up clamp and
`window_truncated` flag (a schedule with no predecessor covers one
interval, ruling 14); a fixed span covers `[t - lag - span, t - lag)`.
An empty window is refused as a conflict naming where coverage stands.
Success advances the watermark (`since_last` only, ruling 9) and, in
every mode, moves a fire cursor that is at or before `t` to the first
boundary after it, so an overdue scheduled run cannot follow with an older
window. The cursor moves at finish, never at claim: a failed manual run
leaves the overdue run to retry. The cadence phase never shifts. A fixed
span shorter than its interval can leave an overdue run's span unread;
fixed spans promise the last span before each run, never contiguity.
Manual runs count toward `max_runs`, are allowed on a disabled schedule,
and record that they were manual. A net with no schedule has no run
control. When ADR-0035's caller execution lands, a caller's run advancing
the shared watermark is sound only while no caller can read less than the
automation key.*
