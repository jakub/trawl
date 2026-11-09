# Bound lateral expansion and account for work after timeout

Status: accepted 2026-09-05, amended 2026-09-07 after re-prep of
[#150](https://github.com/jakub/trawl/issues/150), amended 2026-10-17 by
ADR-0056 (the ping no longer takes a permit), amended (2026-11-08): an export is tracked like a query, see the Amendment.

A short query can spend seconds in DuckDB binding before reading a row.
In a single SELECT list, DuckDB copies an earlier alias's original
expression and binds it again at each reference. A doubling chain makes
preparation exponential while SQL text grows linearly. The original
DuckDB 1.5.5 reproduction exceeded 30 seconds with 266 bytes of DSL. An
interrupt during binding did not stop it promptly. The client's timeout
ends its wait, but the blocking task retains its executor permit and can
prevent unrelated searches or a repin cutover.

The accepted correction strengthens admission accounting and preserves
SQL generation. It ships with deadline and retained-work handling in one
PR. Admission blocks described amplification; accounting reveals work
that still outlives a request.

## Admission counts generated repetition

Use a pure, corpus-independent calculation in `trawl-core`, shared by
`emitter::validate_pipeline` and `stream::compile_stream_plan`. Refuse
conservative lateral expansion above `MAX_LATERAL_EXPANSION = 512`, or
more than `MAX_PIPELINE_STAGES = 128` pipeline stages. Neither limit is
configurable.

The original substituted-DSL-node calculation admits a chain starting
with `a0 = _severity + _severity`, followed by eight assignments of this
form, each naming the preceding alias:

```text
a1 = sev(a0) in (1,3,5,7,9,11,13,15,17,19,21,23)
```

Its original score is 408. The severity set renderer copies its subject
twelve times, once per disjoint range. `sev()` binds its input once inside
each copy, not across the set. These copies compound through the aliases.
Source inspection gives twelve to the eighth seed-expression copies at
the last assignment. This is structural evidence, not measured latency.

Count additional generated-expression nodes caused by substituting
same-stage targets, including operand repetitions and fixed expression
wrappers introduced by translation. Do not parse generated SQL or build
an expanded tree. Numeric rendering profiles describe that work.

Carry a conservative expanded weight `W` and an additional-substitution
count `D`. Ordinary field and literal leaves have `W = 1, D = 0`. An
earlier target reference has `W = W(target), D = W(target) - 1`. For a
rendering with fixed expression-node overhead `c` and child-copy counts
`m_i`, which may depend on source list length and literal values:

```text
W = c + sum(m_i * W(child_i))
D = sum(m_i * D(child_i))
```

Fixed overhead cancels from the paired expanded/unexpanded delta, but
remains in `W` for later references. Take conservative maxima of `W` and
`D` across supported pin interpretations, including unpinned, at each
construct. Never subtract independently maximized expanded and
unexpanded weights: those maxima can describe different renderings.
Propagate `D` directly. Sum assignment deltas across stages and reject
above 512. Reset the alias table at each stage. Match names with
`catalog_key` ASCII folding and assume an earlier same-stage name is
lateral even when a real incoming column might take precedence. No live
catalog lookup affects admission.

Calls, operators, casts, CASE expressions and their expression children,
parameters, literals and references each count as expression nodes.
Parentheses, alias names and syntax tokens do not. Profiles may overcount
fixed overhead but must cover every child copy. Arithmetic saturates;
any saturation means over-limit, never a subtraction of saturated totals.
The checker is linear in source size with a fixed number of pin cases,
uses the fixed severity ladder for range counting, and never enumerates
combinations of catalog assignments. Expressions without earlier alias
references contribute zero lateral expansion, including large flat lists
and independent assignments. Alias-free multiplication introduced by
translation remains outside this measure and is bounded only by existing
query-length and expression-depth limits.

Profiles cover `emitter/expr.rs`, `emitter/functions.rs`,
`emitter/compare.rs`, and expression helpers in `conform.rs`. An invariant
test compares their fixed-node and child-copy bounds with actual emission.
Changing a rendering must fail the test if admission would undercount it.
The expression, function and stage inventories must be exhaustive.

## Stage membership and validity

Walk ordered outputs of `let` and its `eval` spelling, `stats`,
`timechart`, and `eventstats`. Use `projection::agg_output_name` for
explicit and inferred names. Stats and timechart accept scalar function
heads and can express the same chains as let.

Include eventstats conservatively. Its mandatory OVER wrapper currently
causes DuckDB 1.5.5 to reject a substituted nested window before binding
its children. That is not proof that substitution never happens; validity
does not depend on that rejection order remaining unchanged.

Rename substitutes field leaves and has zero delta. Both extraction
arms are distinct cases in the exhaustive match: regex extraction uses
one fixed source per capture, and `extract kv` reads one fixed source
per stage; neither has a recursive alias sequence. Pivot has one USING
expression rather than an output-alias sequence. Other stages have no recursive output
sequence. Record those cases in an exhaustive stage match. All stages
count toward 128, including `from saved` and Rust-tail stages. Search is
not a pipeline stage and has no same-stage aliases. Generated time-bucket
and partition expressions remain outside the recursive alias sequence.

Run the common check first at both doors over the whole pipeline,
including the tail after `extract kv`. This is one admission contract,
not a promise that every admitted stage executes in every lane. Existing
unsupported-stage and semantic errors still apply. Batch, embedded,
hot-only, export, validation and streaming share the diagnostic. Saved
create/update validate before persistence and preserve transactional
schedule-window checks. An older stored query over a cap fails its
attempted scheduled run with the diagnostic; its schedule is not disabled.

Diagnostics name the stage and limit without claiming an exact DuckDB
node count. Render names through `quote_dsl_field` and `sanitize`; if a
name cannot be quoted, omit it and use the stage name alone. For
let, split dependencies across separate `| let` stages. The next SELECT
then reads input columns instead of same-list aliases; this does not
promise execution-time CTE materialization. For stats, timechart and
eventstats, recommend independent outputs followed by separate let
stages for derived values.

512 is retained as the starting limit. The old approximately 0.3-second
observation does not calibrate the corrected measure. The calibration
host is this workspace's AMD Ryzen 7 7800X3D machine, using DuckDB 1.5.5
from duckdb/libduckdb-sys 1.10505.0. Timings on another host are supplemental
and do not satisfy the acceptance criterion. Implementation must capture
bounded addition, scalar-output and severity chains,
ordinary alias reuse, and the split-stage remedy. A failed calibration
criterion is not authority to raise the limit. The stage cap limits
pipeline length separately from corpus width.

## Deadline and physical work lifetime

Create one absolute deadline at authenticated query/export handler entry,
before source resolution and tracking. Carry it through semaphore and
publication acquisition, blocking startup and execution. The deadline
covers the waits a lane actually performs: the resolved-source lane
takes no publication guard today and does not gain one, so gate
membership per lane is unchanged by this work. A scheduler
attempt creates its deadline before its first pool wait. Required handler
awaits use the remaining budget; best-effort history must not delay a ready
result beyond it. This bounds application waiting, subject to runtime
scheduling, not network delivery or non-preemptible CPU work.

A synchronized work-start transition occurs inside the blocking worker
before source discovery or other physical query work. Expiry before it
returns 503 with a safe capacity/preparation reason. Expiry after it is
the existing 504 query timeout. The worker checks deadline and latched
cancellation at that transition. Owning a permit does not mean work
started. An early parse/source error finishes tracking exactly once.

Register lifecycle state before dispatch. The request future does not
own final cleanup: timeout, caller drop, startup failure, panic and
completion converge on cleanup independent of that future. Bound the
interrupt-handle wait too. Cancellation latches before a handle exists.
A queued blocking closure can retain a permit after a 503 until cancelled
before start or allowed to run cleanup. Returning 503 is not evidence
that the permit is free.

Keep the permit and publication guard until physical work stops. Keep
interrupt registration until cleanup. Serialize interrupt invocation with
deregistration and executor reuse so an old handle cannot interrupt the
next query. Repeated cancellation is accepted while work exists; an
acknowledgement means requested, not stopped. Check latched cancellation
at the bind-to-execute boundary rather than promising an earlier interrupt
survives every API phase.

Apply the lifecycle to normal/resolved-source queries, all export pool
paths, scheduler work, and helper tasks that can retain capacity. HTTP
work retains the exact submitting key ID. Preserve existing list and
cancellation permissions; system work has no human owner and remains
admin-cancellable. Do not add permissions or expose system query text to
ordinary query readers. Record a request outcome once, separately from
physical completion; reclamation never rewrites timeout history.

`GET /queries` adds a retained list with ID, existing permitted display
metadata, work-start state and retained duration. System entries visible
to non-admin readers contain only ID, work kind and timing. Owner IDs stay
internal. Retained work is not also counted as an ordinary active request.
`pool_active` continues to count all held permits; `pool_retained` is its
subset, exposed through stats, dashboard snapshots and the
`trawl_query_permits_retained` gauge. The terminal panel shows
`active: 3/4 (1 retained)` when nonzero. Lifecycle logs
`query_permit_retained` and `query_permit_reclaimed` contain metadata,
never DSL text or principal metric labels.

Ping has a 250 ms total probe budget. Field sampling has at most 1 second
for acquisition, also bounded by its existing overall query deadline.
Exhaustion returns 503 or the existing unhealthy response carrying that
safe capacity failure. Both use the same cleanup rule. `exclusive()`
keeps its bounded acquisition and logs the retained count on failure.
No replacement executors enlarge the pool around retained work. A
scheduler capacity failure is a failed attempt; later runs follow the
existing schedule.

## Scope and residual risk

One PR covers admission, deadlines, retained work, tests and operator
docs in this repository. No emitter rewrite, process isolation, separate
bind semaphore, concurrency reduction, new permission or configurable
limit. Query results and parameter ordering remain unchanged.

Evidence includes boundary and cross-lane tests, emission-accounting
invariants, bounded DuckDB probes, deterministic lifecycle races,
authorization tests and committed Ratatui snapshots linked with CI.
Browser-checkable terminal evidence does not require a new browser page.

Alias-free multiplication introduced by SQL translation, corpus width,
file discovery, high-cardinality pivot and other database planning work
remain outside this static alias bound. This design does
not prove a universal bind-time deadline or make an in-process binder
killable. It narrows ADR-0011's permit claim: concurrency limits work in
flight, admission limits described amplification, and retained accounting
reveals occupied capacity after the request ends.

## Amendment: an HTTP request count is not an executor permit, 2026-10-08

ADR-0054 adds a server-wide count of HTTP requests in progress. A query's count ends with its request. Its executor permit may stay retained after that, as this ADR describes. The two are separate charges with separate lifetimes, and the HTTP count does not follow retained executor work. Only blocking work that holds a request's body, which is ingest decode, the ingest WAL write and the preview report, keeps its request's count until that work ends.

## Amendment: the ping no longer takes a permit, 2026-10-17

ADR-0056 replaces the request-triggered ping. The sentence "Ping has a 250 ms total probe budget" and the 503 on its exhaustion no longer describe health. A dedicated thread samples `SELECT 1` every 10 seconds on a diagnostic connection cloned from the query database, outside the executor pool, with a 250 ms budget per sample. Health reads the latest sample and never waits for a permit, so a full or retained pool cannot fail it. Field sampling keeps its 1 second acquisition bound and its permit. The retained-permit rules above apply to every permit the pool still issues.

## Amendment (2026-11-08): an export is tracked, counted and recorded like a query

The untangle prep of 2026-11-08 (defect D15) found that `POST /api/v1/export` runs pool work under its own query id but opens no tracker entry, records no history and does not count itself (`handlers.rs:3354`). `/query` does all three (`handlers.rs:103`, `:138`, `:370-404`), and so does a capture (`results/service.rs:590`, `:696`, `:901`). A long Parquet export is invisible in `GET /api/v1/queries`, cannot be cancelled there, and `total_queries` undercounts. The human ruled on 2026-11-08:

> Q: D15: exports (`trawl export`, the Export button) do not show in /queries, query history, or the total query count; queries and captures do. Is that deliberate?
> Options offered: No, track them (Recommended) / Yes, deliberate
> A: "No, track them (Recommended)"

**An export in every format follows the `/query` account.** After it stamps its deadline, the export increments `total_queries` once. It opens a tracker entry under the query id it already allocates, before the DSL check, so the entry lists the export while it waits. One completion point closes the entry exactly once: complete with the exact exported row count, fail with the public message ([ADR-0040](0040-every-server-failure-names-its-request-and-stage.md), amendment of 2026-11-08), or time out. A request that ends without an outcome records `ABANDONED` through the tracker guard. Each explicit outcome increments `trawl_queries_total` once with the existing `status` labels. A success writes one best-effort row to the key's query history, under the original deadline, as `/query` does. A failed or expired history write never turns a ready export into an error. Success means the body is ready, not that the client received it.

**The Parquet row count is exact and costs no second run.** Query history needs a row count. The export reads it from its staging table or from the finished file's metadata. A byte length, the row limit or a placeholder zero would write false history.

**Cancellation is the same as for `/query`.** `DELETE /api/v1/queries/{id}` applies its existing authority check and interrupts through the pool registry the export already uses. As for `/query` today, a cancel that arrives before the pool registers the work returns false (`pool.rs:277-280`), and a cancel during CSV or NDJSON rendering has nothing to interrupt. That gap belongs to the pool and every route that uses it, not to exports.

**Visibility and grants.** Every key with `query` now sees export DSL among active and recent entries, which follows from the ruling. A key with only `export` gains no listing and no cancel grant. The permissions this ADR preserves stay as they are. Captures keep their order (count first, tracker after capture admission). Scheduled runs and live-tail setup keep their counters unchanged.

### Considered, not adopted

- **One shared run object for every route that runs pool work.** Rejected for this fix: the routes do not share an order. A capture tracks its executed text after planning, the scheduler owns system work and run rows, and live-tail setup is unbounded. The export change stands alone.
- **A request-level stop latch for exports** that covers the wait before registration and the render phase. Rejected: `/query` has the same gap, so an export-only latch makes the routes diverge again and adds races between cancel and completion.
- **Move the `total_queries` increment into `tracker.start`.** Rejected: `tests/results_admission.rs:580-596` uses the capture's early increment as its arrival barrier.
- **A `kind` field on `/queries` entries.** Rejected: a wire addition the ruling does not need.

### Premise checks at `b71f9e6e`

- `handlers.rs:3354-3460`: the export handler allocates and records its query id (`:3376-3377`), builds a `refused` closure for its timing account (`:3412`), runs the DSL check (`:3422`) and Parquet export (`:3433-3444`). It never calls `tracker.start`, `total_queries` or `history.record_query`.
- `store/history.rs:91-98`: `record_query` takes `row_count: usize`.
- `trawl-engine/src/executor.rs:1850-1908`: Parquet export stages its bounded result in `__trawl_export` before `COPY`.
- `pool.rs:4996-5005`: `every_lane_registers_its_kind_and_owner` already parks `export_parquet` at `Seam::Started`.
- `results/service.rs:931-965`: captures already increment `trawl_queries_total`.
