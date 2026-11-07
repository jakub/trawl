# Retention and report windows: age is per-env policy, pressure is survival, the schedule owns its window

status: accepted (2026-08-18) — prep ruling record for #97; amended (2026-10-22) by ADR-0057 and ADR-0059: captured results and kept evidence are a new retention class; amended (2026-10-23): the capture sweep without the scheduler; amended (2026-11-06): stale runs at boot, retention of terminal runs only, and a lifetime run budget

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

## Amendment: captured results and kept evidence, 2026-10-22

Two stores join retention with their own clocks, neither under the per-env age policy nor under pressure deletion. A captured result ([ADR-0057](0057-a-capture-is-an-immutable-expiring-result-that-later-reads-page.md)) expires 15 minutes after creation and is bounded by per-key and installation quotas; it lives in the results root, not the data root, so the pressure sweep never sees it. Kept evidence ([ADR-0059](0059-an-investigation-is-grant-visible-and-its-evidence-outlives-the-corpus.md)) lives 7 days after the investigation's last change and never past 30 days after its creation unless `server_manage` extends it, under a 64 MiB per-investigation and 1 GiB per-installation budget reserved before scanning. The hourly sweep that prunes report runs also prunes both; it deletes files and leaves metadata, so a kept row reads as pruned. Ruling 12 stands: `from saved` receives no window, and a capture of it records that it has no bounds.

## Amendment: the capture sweep without the scheduler, 2026-10-23

The hourly retention tick that sweeps captured results belongs to the scheduler, so an installation with `[scheduler] enabled = false` has none. There trawld runs the same sweep on a task of its own, every `[capture] ttl_secs` clamped to between one second and one hour. With the scheduler on, the retention tick sweeps and no second task runs. Either way, a read that finds a temporary capture past its deadline marks it expired and removes its file at once, the binding quota pass expires due captures before it counts usage, and boot expires the captures that came due while trawld was down.

## Amendment: stale report runs are marked at boot, 2026-11-06

A run that was `running` when trawld stopped never finishes. Until now only the scheduler task marked such runs, so with `[scheduler] enabled = false` the row stayed `running` and every manual run on that schedule was refused as "already in progress" (#349). Boot now marks them, whatever `[scheduler] enabled` says: `boot::prepare_corpus` sets every `running` report run to `error` with the message `interrupted by server restart`, beside the investigation steps it already interrupts, on ingest and query-only nodes alike. The advisory lock is held by then, so no live sibling owns those rows.

Boot is the only place that marks stale runs. The scheduler loop no longer does it: its cleanup ran on a spawned task after the listener could already accept a manual run, and it could mark that fresh run `error` and free the schedule for a second claim while the first still executed. A failure to mark them stops the boot, like a failure to interrupt the steps. A daemon that cannot prove its schedules are free must not accept claims on them. Marking leaves the watermark and the fire cursor where they are, as any failed run does (ruling 9).

## Amendment: retention deletes only finished runs, 2026-11-06

Report-run retention deletes only terminal runs: `success`, `error` and `timeout`. A `running` row is never deleted by age and never counted or deleted as excess, so retention cannot free a schedule's one running slot while its run executes (#350). The per-schedule ranking orders terminal runs by `started_at DESC, id DESC`, so `max_runs_per_schedule` keeps that many terminal runs and a running row beside them costs nothing.

Both knobs must be at least 1. `report_retention_days = 0` and `max_runs_per_schedule = 0` stop the boot with a configuration error naming the key, rather than meaning "keep none". `report_retention_days` is at most 36500, about 100 years: the cutoff is a date the scheduler must compute, and a larger value used to clamp silently to 365 days or panic the scheduler task. `max_runs_per_schedule` has no upper bound, because a count needs no date arithmetic.

## Amendment: `max_runs` is a lifetime budget, 2026-11-06

`max_runs` counts the runs a schedule has claimed over its life, not the runs retention has kept (#351). Counting retained rows let retention refill the budget: a daily schedule with `max_runs = 60` under the 30-day default never stopped. The schedule now stores `runs_claimed`, and every claim, scheduled or manual, increments it in the same transaction that inserts the run, only while `max_runs` is absent or `runs_claimed < max_runs`. A claim that fails to insert rolls the increment back. A claimed run that later fails, times out or is interrupted keeps its claim. There are no refunds.

Editing a schedule keeps `runs_claimed`. Raising `max_runs` lets the schedule claim again; lowering it below `runs_claimed` refuses new claims and leaves a running run to finish. Only deleting the schedule and creating a new one starts a new budget. A schedule is exhausted when `max_runs` is set and `runs_claimed >= max_runs`. Exhausted is derived from those two values and is never stored. `max_runs = 0` stays legal and makes a schedule exhausted from the start. The API reports `runs_claimed` as the schedule's `total_runs` and adds `exhausted`. Counts that describe kept runs, such as the run statistics and the run list's total, stay retained counts and say so.

The upgrade seeds `runs_claimed` from the runs each schedule still has. That is a lower bound: a schedule whose older runs retention already deleted may claim up to its cap again before it stops. History that retention deleted cannot be counted. Seeding every capped schedule as spent would also stop schedules that never reached their cap, so the seed is only what the kept rows prove.
