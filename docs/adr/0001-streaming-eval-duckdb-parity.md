# Streaming eval mirrors DuckDB scalar-function semantics

**Status:** Accepted; amended 2026-10-17 (#304): aggregation caps are reported
live and refused in batch, see the Amendment; amended (2026-11-08), see the
Amendment (2026-11-08)

trawl has two expression evaluators that must produce the same answer for the
same DSL: the **batch path** (`emitter/` → DuckDB SQL → DuckDB) used by
`/query`, and the **streaming path** (`eval.rs`, in-memory over a
`serde_json::Map` event) used by live tail / SSE. A query that computes a field
in the editor must compute the *same* field when streamed. They had silently
drifted — several scalar functions emit correct SQL but fell through
`eval_scalar_fn`'s `_ => EvalValue::Null` catch-all, so `let`/`eval` over them
returned the right value in batch and a silent `null` in live tail (#22).

## Decision

Streaming eval is defined as a faithful in-memory reimplementation of the batch
path's scalar semantics. Where the two cannot match exactly, the divergence is
deliberate and documented here.

- **Timestamps are `EvalValue::Timestamp(NaiveDateTime)`.** trawl's `timestamp`
  column is cast `AS TIMESTAMP` (timezone-naive) on both the hot path
  (`executor.rs`) and during compaction (`compaction.rs`). DuckDB's
  `… AS TIMESTAMP` cast *discards* any offset and keeps wall-clock components.
  We mirror that: a naive datetime, with input offsets parsed then dropped.
  `now()` returns `Utc::now().naive_utc()`. `typeof` reports `"TIMESTAMP"`.
- **String→timestamp coercion accepts DuckDB's practical ISO set** ('T' or space
  separator, optional fractional seconds, optional offset which is discarded,
  date-only → midnight). Unparseable strings coerce to `Null`. Comparisons
  (`eval_cmp`/`eval_eq`) coerce a `Str` against a `Timestamp` via `as_timestamp`,
  falling back to the prior `Str`-vs-`Str` path when parsing fails (no regression
  for non-timestamp strings).
- **`date_diff` replicates DuckDB's per-unit count exactly.** For year, quarter,
  month, day, hour, minute and second this is a boundary-crossing count, not
  floored elapsed time (`date_diff('hour','…23:59:59','…00:00:00') == 1`). The
  one exception is `week`: DuckDB computes it as integer `days / 7` (verified
  against live DuckDB, pinned by the parity test), *not* a week-boundary count.
- **The date/time unit vocabulary is a validated allowlist** — `{year, quarter,
  month, week, day, hour, minute, second}` (plus `dow`, `doy`, `epoch` for
  `date_part`). The unit must be a string literal; unknown units and non-literal
  units are rejected at emit time in **both** paths, so an unsupported unit can
  never error in batch while silently nulling in live tail.
- **The contract is enforced by tests**, not by discipline:
  - `eval_scalar_fn` returns `Option<EvalValue>` — `None` = "not a handled
    scalar" (distinct from `Some(Null)` = "handled, evaluated to null"). A
    coverage test asserts every non-aggregate `KNOWN_FUNCTION` returns `Some`,
    so the catch-all can never silently swallow a known scalar again.
  - A behavioral parity property test (modelled on `tests/filter_parity.rs`)
    runs random scalar expressions through both eval and DuckDB and asserts
    type-aware normalized equality (epsilon floats, timestamps compared as
    parsed `NaiveDateTime` instants). It excludes `now()` (nondeterministic),
    which gets a dedicated unit test.

## Considered and rejected

- **`EvalValue::Timestamp(DateTime<Utc>)`** — absolute-instant correctness, but
  it *normalizes* offsets, so it would diverge from the batch path for
  offset-bearing input strings (`14:00+02:00` → `12:00Z` vs DuckDB's `14:00`),
  recreating the very batch-vs-live gap #22 exists to remove.
- **Floored-elapsed `date_diff`** — simpler, but diverges from batch at every
  sub-unit boundary straddle.
- **Implementing DuckDB's full unit vocabulary** (decade, century, millennium,
  isoyear, microsecond, …) — large, fiddly calendar math for units no log query
  uses; the validated allowlist gives parity at a fraction of the cost.
- **Validating strftime/strptime format codes against an allowlist** — unlike
  the closed unit enum, format strings are free-form and even "valid" `%`-codes
  can differ subtly cross-engine (padding, locale, `%Z`). Validation would give
  false confidence at real cost. Instead we pass the format through to chrono,
  parity-test the common C-strftime subset, and accept that exotic/locale codes
  follow chrono.

## Consequences

- Adding a scalar function to the emitter now *requires* adding it to
  `eval_scalar_fn` — the coverage test fails otherwise. This is the point.
- Exotic/locale `strftime`/`strptime` format codes (`%c`, `%Z`, …) follow
  chrono and may differ from DuckDB. Documented in the DSL reference.
- Issue #25 closed the residual `strptime` partial-format gap: streaming now
  resolves via `chrono::format::Parsed` and fills omitted components from the
  `1900-01-01 00:00:00` base exactly like DuckDB (year-only, year-month,
  month-day, and date-plus-incomplete-time). The common date/time partials are now
  at parity; the remaining divergences are the exotic/locale codes above plus a
  bare two-digit year (`%y` alone, which nulls) and offset codes (`%z`/`%Z`, which
  keep chrono's wall-clock time) — both follow chrono.
- `Timestamp → VARCHAR` rendering (`tostring`, and `From<EvalValue> for Value`
  when a `Timestamp`-valued `let` is serialized into the event) must match
  DuckDB's timestamp text format (space separator, microsecond precision,
  trailing-fraction trimming) — pinned by the parity test.

## Amendment: aggregation caps are disclosed, 2026-10-17

Issue #304 (the ADR-0056 limits review) found a second silent divergence. The
streaming accumulators stop taking input at three caps: 10,000 groups (per
bucket for a `timechart`), 10,000 distinct values per `dc()` or `values()`, and
100,000 values per `median` or percentile. DuckDB has no such caps, so a
capped live aggregate answered differently from the same DSL in batch, and
nothing said so. The human ruled on 2026-10-17 that the caps keep their values
and that the divergence is disclosed.

- **The aggregation records which caps dropped input.** `trawl-core`'s
  `AggregationCap` names the three caps and their limits, and
  `CompiledAggregation::capped` lists the ones that dropped input. A cap is
  marked only when input is dropped: a new group while the map is full, an
  unseen value while the set is full, a number while the store is full.
  Reaching a cap exactly marks nothing. A mark lasts for the life of the
  aggregation, because the dropped input does not come back.
- **Live tail reports it on every snapshot.** Each aggregate `snapshot` carries
  `capped`, a list of `{cap, limit}` with the names `groups`,
  `distinct_values` and `exact_values` (`trawl_api::stream`). The field is
  omitted while nothing is dropped. The limit travels on the wire, so no
  client hard-codes it. An event dropped at a cap still counts toward the next
  snapshot, so the scheduled snapshot discloses a cap even when no more input
  arrives. The web UI shows a persistent "Live aggregate is partial" notice
  above the results. The TUI marks the results title in the theme's warning
  colour. Neither turns the result into an error.
- **The batch tail refuses.** The Rust post-stages behind `extract kv` run the
  same accumulators over the SQL prefix's rows, but a query response has no
  field for a partial answer. An aggregation there that would drop input
  refuses with `400 result_too_large`, and the message names the cap and its
  limit (`EngineError::AggregationCapped`). A cut aggregate is a wrong answer,
  not a short one, so the batch lane does not return it.
- **`top`, `rare` and the timechart bucket count stay uncapped.** Bounding them
  needs its own design (human ruling).

Considered and rejected:

- **A separate SSE event for the cap.** A snapshot replaces the client's whole
  view, so the report rides with the rows it qualifies and survives a
  reconnect without a second ordering rule.
- **Returning the batch answer with a warning.** The query response has no
  warning channel for this, and adding one would still hand scripts and
  scheduled reports a wrong number.
- **Raising the caps or making them configuration.** They bound per-stream
  memory, and nobody asked for a key.

The same review found that `drop` of a field that no event carries refused in
batch and passed every event live. Batch now ignores the field too
([ADR-0056](0056-queries-spill-into-a-bounded-scratch-directory-and-health-samples-the-engine.md)'s
amendment of 2026-10-17).

## Amendment (2026-11-08): aggregates are a closed set, and an aggregate stands only where both lanes can run it

This amendment records the untangle prep of 2026-11-08 (defects D7 and D12). The enforcement bullet above covers scalar functions: the coverage test makes the catch-all in `eval_scalar_fn` unable to swallow a known scalar. Aggregates had no such guard, and two silent divergences followed from it.

- **An aggregation slot fell through to a row count.** The live accumulator constructor ends in `_ => AccState::Count(0), // fallback` (`crates/trawl-core/src/stream.rs:2108`). `stats lower(x)` passes the slot check, because a scalar head is legal in batch (`emitter/validate.rs` and `sev_is_not_an_aggregate`, `emitter/mod.rs:2992`). It then counted rows on the live lane and on the kv batch tail, which compiles through the stream plan (`crates/trawl-engine/src/post_process.rs:209`).
- **An aggregate in a row-scalar position was NULL live.** `| where count(x) > 1` and `| let y = avg(x)` pass the live name and arity check (`stream.rs:1211-1214`) and reach `eval_scalar_fn(..).unwrap_or(EvalValue::Null)` (`crates/trawl-core/src/eval.rs:342`). No core check refused them in either lane. Batch left it to DuckDB's binder.

The enforcement bullet gains three rules:

- **The aggregates are one closed enum in trawl-core.** It parses from a name and maps the aliases (`dc` and `distinct_count`, for example). `is_aggregate_function` (`emitter/functions.rs:523`) derives from it. The SQL aggregate translator and the live `new_acc_state` (`stream.rs:2073`) both match the enum exhaustively, with no wildcard arm. A new aggregate does not compile until both lanes implement it.
- **An aggregate in a row-scalar position is refused before either lane compiles.** Row-scalar positions are `where`, `let`, `sort` and every other per-row expression, at any depth, including inside a scalar call. An aggregate nested inside another aggregate's operand (`stats sum(count(x))`) is refused the same way. A scalar wrapped around an aggregate output in an aggregation slot is not nested in an operand, and keeps its batch meaning. Both lanes return one sentence for each refusal, for example "count() is an aggregation; use it in stats, timechart, eventstats or pivot". A field named `count` stays legal.
- **A scalar head in an aggregation slot stays legal in batch and is refused live.** ADR-0024 says stats and timechart accept scalar function heads, and `sev_is_not_an_aggregate` pins `* | stats sev(level)` as a projection without `GROUP BY`. The live compiler has no implementation for a scalar head, so it refuses one with an explicit `UnsupportedStage`, never a count. The refusal also covers stages after `extract kv` in batch, because that tail compiles through the stream plan.

The slot check covers `eventstats` as well as `stats`, `timechart` and `pivot`; `validate_pipeline` skipped it before (`emitter/validate.rs:65`, `_ => {}`). New expression walks use an explicit work stack, as ADR-0061 requires for whole-tree walks, because a flat chain can be deeper than a worker stack allows. The `stream.rs` comments that say "this lane never runs `validate_pipeline`" are corrected: `/api/v1/stream` runs it through admission (`crates/trawl-server/src/admission.rs:87-93`).

A stored query that one of these refusals now catches fails its next validation or scheduled attempt with the diagnostic, and the scheduler records the failed run and continues. ADR-0061 names the backup pair that restores the old behaviour.

Considered, not adopted:

- **Refuse every scalar head in every lane.** It breaks `stats sev(level)` and the ADR-0024 contract in batch.
- **One lane-neutral validator over the whole query, with an exhaustive stage match, in the same change.** It is the right destination: it deletes the hand mirrors in `stream.rs:72-90` and the `_ => {}` arm in `validate_pipeline`. Removing several mirrors in the change that also alters admission makes a dropped mirror hard to bisect, so it needs its own change. This prep records it and files nothing for it.
- **A full function registry that also owns SQL rendering, result pins and cost profiles.** It is larger than the defect and is not needed to close either divergence.

Premise checks at `b71f9e6e`: `stream.rs:2073-2108` (string descriptor, wildcard `Count(0)`); `stream.rs:1211-1214` (name and arity only); `eval.rs:340-342` (unhandled call is NULL); `emitter/expr.rs:23` and `emitter/functions.rs:287-298` (the general translator emits any known aggregate wherever it appears); `emitter/mod.rs:2989-2998` (`sev_is_not_an_aggregate`); `post_process.rs:209` (the kv tail calls `compile_stream_plan_with`). The DuckDB binder's answer to `where count(x) > 1` was not reproduced; the refusal no longer depends on it.
