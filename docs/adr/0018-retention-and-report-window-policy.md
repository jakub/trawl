# Retention and report windows: age is per-env policy, pressure is survival, the schedule owns its window

status: accepted (2026-08-18) — prep ruling record for #97; amended (2026-10-22) by ADR-0057 and ADR-0059: captured results and kept evidence are a new retention class; amended (2026-10-23): the capture sweep without the scheduler; amended (2026-11-06): stale runs at boot, retention of terminal runs only, and a lifetime run budget; amended (2026-11-08): ruling 8's pruning claim, see the Amendment; amended (2026-11-09): scheduled runs in flight are capped at half the executor permits, see the Amendment; amended (2026-11-11): manual runs are capped too, running a net now needs `query`, and the tick lists only due schedules under a deadline, see the Amendment

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

## Amendment: ruling 8 prunes by ingest hour, not by `_time`, 2026-11-08

Prep record for the untangle prep of 2026-11-08 (defect D3). ADR-0013's
amendment of the same date holds the full record and the human's ruling.

Ruling 8 says "the layout prunes by" `_time`. That was never true. A
directory names the hour compaction wrote it (`ingest/compaction.rs:4783-4787`).
Only `last=` prunes by directory: it walks the hours from `now - N - padding`
to now (`source.rs:527-551`). Absolute windows, and the materialized report
windows of ruling 11, select every date directory and filter by `_time` in
SQL (`source.rs:117-136`, `report_window.rs:609-633`).

`last=` pruning is sound because ingest now bounds `_time` from above: a
`_time` more than 15 minutes after arrival is replaced by arrival with the
repair code `time.ahead_of_arrival`. The padding stays one hour, at least that bound.
The axis of ruling 8 stays `_time`. Rejecting `_ingested` windows still holds,
because a `_ingested` window would still disagree with interactive queries.
Retention keeps deleting by directory date. With the bound, no row has a `_time`
more than 15 minutes past the end of its directory's hour, so age-based expiry never
deletes a row before its own `_time` has nearly reached the age limit.

## Amendment: scheduled runs in flight are capped at half the executor permits, 2026-11-09

Prep record for the prep of 2026-11-09 (#407, from #343). Each scheduler tick claims every due schedule it can and spawns one task for each claim (`scheduler.rs:221-299`). Each task holds its resolved query text while it waits for a preparation slot and then an executor permit. Nothing bounds how many wait at once except the number of schedules, and the preparation allowance is first come, first served (ADR-0063), so a person's search queues behind every due run. The tick also reads every enabled schedule with its saved query text (`store/schedule.rs:999-1015`) before it claims anything.

The human ruled on 2026-11-09: "Half the executor permits (Recommended)", at least 1.

**At most `max(1, executor permits / 2)` scheduled runs are in flight.** The scheduler takes a lease before it claims a due run and claims nothing when no lease is free. A claim that does not start a run gives its lease back. A run keeps its lease until its physical work ends, not until its task ends: a preparation job or executor work that outlives a timed-out request (ADR-0024, ADR-0063) still holds it. On a pool of 4 permits, 2 scheduled runs run at once, and interactive work can still take the other 2. Manual runs keep their HTTP admission and take no lease.

**A run the cap defers is a missed run, and ruling 9 already covers it.** The scheduler leaves the deferred schedule's fire cursor, watermark, run table and `runs_claimed` as they are. A later tick claims it, and its missed boundaries coalesce into one window under `max_catchup_intervals`, as after any other missed run.

**The tick reads schedules oldest due first, in bounded pages, without query text.** A page holds schedule ids and the fields the tick needs before a claim, ordered by fire cursor and then id, so a full cap cannot starve the schedules with the highest ids. Every value a run depends on is still read again under the claim's row locks.

The cap bounds automatic work in flight. It does not cap how many schedules exist, and it does not give interactive work strict priority. The cap counts executor permits, not preparation slots, so where the preparation allowance equals the cap, as on a 4-core host with 4 permits and 2 slots, scheduled runs can still hold every slot for the length of their preparations. On a pool of one permit the one lease and the one permit are the same capacity.

Considered, not adopted:

- **One fewer than the preparation slots.** It keeps a slot free for interactive work on hosts with two or more slots, but it allows only one scheduled run on a host with two slots. The human chose the executor basis.
- **Reserve preparation slots for interactive work, or cap the schedule count.** Each needs its own policy and neither bounds the tasks the scheduler holds.
- **Release the lease when the task ends.** A timed-out run's physical work would keep its permit while a replacement starts, so the cap would leak.

## Amendment: manual runs are capped, running a net now needs `query`, and the tick lists only due schedules under a deadline, 2026-11-11

Prep record for #444, the follow-up the human accepted from #407's security review on 2026-11-10. The 2026-11-09 amendment left manual runs out of the cap: "Manual runs keep their HTTP admission and take no lease." That admission ends at the response head (ADR-0054), while the run goes on detached (`handlers.rs:3263-3278`). A key may own any number of nets, and only one run per schedule is guarded (`report_runs_one_running`), so one key could press Run now on every net it owns and hold every preparation slot and executor permit. The tick also listed spent schedules and schedules whose key cannot run them on every poll, through a scan and a sort of the due set per 64 rows, because the only index on `schedules` is on `enabled`. And `trigger_run` and `get_report_run` checked only `saved_query`, while a scheduled run needs `query` and `saved_query` (`scheduler.rs:405-445`). So a role with `saved_query` alone could save any query, run it now, and read its rows.

The human ruled on 2026-11-11:
- the grant gap: "Fix it in #444 (Recommended)";
- the manual cap: "Separate gate (Recommended)";
- one change, not two: "One PR";
- the limits below that remain: "Accept, note them (Recommended)".

**A manual run takes a run lease from its own gate before it is claimed.** The gate has two limits: one manual run per key, counted on the caller's key, and `max(1, executor permits / 4)` manual runs across the installation. It is separate from the scheduled cap, so Run now never defers a schedule and a busy scheduler never refuses Run now. `trigger_run` checks the grant, then ownership (404), then the corpus gate (503), then tries the gate, and only then claims. It never waits. A refused run answers 429 `manual_run_busy` with an advisory `Retry-After` of at least one second. It spends no budget, writes no run row and moves no cursor. A claim that starts no run (400 or 409) gives its lease back. While a key's own manual run is running, pressing Run now on the same net answers 429, not the 409 for a run already in progress.

**A run lease lasts until the last of its run's work stops.** That covers the window resolution at claim time, the preparation job, the executor work and the run's own result writing and finishing, as the scheduled lease already did. A timed-out or abandoned run keeps its lease while any of that work survives. The claim and the run share one task: the request waits for the claim's answer, and a request dropped after the claim commits does not leave a running row with nothing to run it.

On P executor permits the two gates hold at most `max(1, P/2) + max(1, P/4)` runs: 2 runs on 1, 2 or 3 permits, 3 on 4, and 6 on 8. Interactive work keeps the rest. On 4 permits that is 1 permit, where the 2026-11-09 amendment gave it 2. On 2 permits or fewer the runs can hold every permit. Preparation slots are still not reserved.

The lane counts the caller's key, not the schedule's key. ADR-0035 runs a manual run as the caller, so the 1.1 automation key does not rekey this lane.

**Running a net now and reading a run's rows need `query` as well as `saved_query`.** `saved_query` alone still creates, edits and deletes nets, sets and removes schedules, and lists runs with their metadata. Saving text runs nothing. Under ADR-0025, Run now and the run rows view are absent in the web UI for a key without `query`.

**The tick lists only schedules that can still claim, oldest first, through an index.** A due schedule is enabled, its fire time has come, and its run budget is not spent. The listing's predicate and a partial index on `(next_fire_at, id)` carry the same budget condition. Exhaustion stays derived and is never stored, so raising `max_runs` lists the schedule again at the next tick. A schedule whose key cannot run it, or whose window fails to resolve, stays due with its cursor and watermark untouched (ruling 9). The tick remembers an unusable key for the rest of that tick and logs one skip per key with a count of its schedules. It does not remember a usable answer: every claim checks its key live, so a revocation takes effect at the next claim.

**Listing a page and each claim run under a 5-second deadline before commit.** Every statement takes its timeout from what is left of that deadline. On expiry the call writes nothing: the cursor, the watermark, `runs_claimed` and the run table stay as they are.
- A scheduled claim that expires gives its lease back, and the tick goes on to the next schedule.
- A listing that expires ends that tick.
- A manual claim that expires answers 503 `service_unavailable` and gives its lease back.

A COMMIT already sent is awaited, never cut: a lost answer is not proof of a rollback. The value is fixed, with no setting.

**Two gauges per origin show how full each gate is:** run leases in use and the lease limit, labelled `scheduled` or `manual`, read at scrape time. No log line is written per tick.

The human accepted these limits that remain:
- A COMMIT already sent can still stall the tick, and a lost commit answer holds its lease until it resolves.
- Schedules whose key cannot run them still cost one indexed page read per 64 and one key check per distinct key on every tick.
- Nothing bounds a whole tick. That needs a design for settling an unknown commit and a fair way to resume, and this amendment provides neither.

The listing index is persisted state. A binary older than its migration refuses to start against the upgraded database (`store/migrations.rs:195-209`). To go back, restore the app-state database backup taken before the upgrade, with its matching `data/scheduled` tree.

Considered, not adopted:

- **Share the scheduled cap with manual runs.** It keeps the total at `max(1, P/2)`, but a busy scheduler then refuses Run now, and a few keys that keep pressing Run now defer every schedule with no signal. The human chose separate gates.
- **Hold the HTTP request count for the run's life.** ADR-0054 keeps independent work out of that count, and the count returns at its deadline while physical work can outlive it.
- **Wait for a place.** It holds the request open, or it queues detached tasks again.
- **Store an exhausted flag, or move a never-startable schedule's cursor.** Exhaustion is derived (the 2026-11-06 amendment), and moving the cursor breaks ruling 9.
- **Remember a usable key across the tick.** It delays a revocation past later claims in the same tick.
- **A separate lock timeout, a config key for either limit or the deadline, and a log line when the cap is full.** Nobody asked for them, and the gauges answer the question without writing to the corpus every tick.
- **Cap how many schedules a key may own.** It needs its own policy, as in the 2026-11-09 amendment.

Premise checks at `4dd30bf2`: `handlers.rs:3169,3300` (both routes check only `saved_query`); `scheduler.rs:405-445` (a scheduled run needs both grants); `handlers.rs:3199-3278` (claim, then spawn with no lease, then answer); `pool.rs:247-268,2777-2787` (the scheduled cap, its lease and its try); `store/schedule.rs:1020-1041,1387-1403,2412-2460` (the listing has no budget term; an exhausted claim rolls back); `migrations/20260913000001_initial_schema.sql:35-40,278,286-287` (indexes on `schedules`, one running run per schedule); `store/investigations.rs:1403-1624` (the `WriteTx` deadline, its commit rule, and lock and statement timeouts mapped to `WaitExpired`); `store/migrations.rs:195-209` (an older binary refuses a newer ledger); `web-ui perms.rs:65` (`can_query`).
