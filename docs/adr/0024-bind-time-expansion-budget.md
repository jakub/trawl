# Bound lateral expansion and account for work after timeout

Status: accepted 2026-09-05, amended 2026-09-07 after re-prep of
[#150](https://github.com/jakub/trawl/issues/150).

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

Rename substitutes field leaves and has zero delta. Regex extraction
uses one fixed source per capture; pivot has one USING expression rather
than an output-alias sequence. Other stages have no recursive output
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
publication acquisition, blocking startup and execution. A scheduler
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
