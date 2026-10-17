# Streaming eval mirrors DuckDB scalar-function semantics

**Status:** Accepted; amended 2026-10-17 (#304): aggregation caps are reported
live and refused in batch, see the Amendment

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
