# Scalar statement semantics: one mirror per value domain, one instant per unit of output

status: accepted (2026-08-18) — prep ruling record for #93; amended (2026-11-08), see the Amendment

The DSL evaluates in two lanes: DuckDB SQL (batch) and the in-memory Rust
evaluator (SSE live tail, and the `rust_stages` batch tail behind
`extract kv`). ADR-0001 made batch the contract and streaming the mirror;
ADR-0011 built the probe discipline (`compare.rs`/`conform.rs`/
`pin_match.rs` pinned by `duckdb_probe.rs`). The #93 audit found `eval.rs`
had quietly grown value-domain rules of its own: a second, unprobed
timestamp parser that strips malformed `+HH:MM` offsets without validating
them; an ADR-0001-blessed lexical fallback when string→timestamp coercion
fails; `tonumber(bool)` disagreeing with `TRY_CAST(... AS DOUBLE)`;
`now()` read per call (a `let` and a `where` in one statement can see
different instants); Int/Int division truncating where DuckDB's `/` is
true division; unguarded `+ - *` overflow (debug panic in the live lane);
non-empty-string truthiness in `if()` where DuckDB rejects; `ceil`/`floor`
returning integers where DuckDB returns DOUBLE. The parity harness could
not see any of this: infrastructure failures counted as Skip, matchers
collapsed timestamps to whole seconds, whole case families were never
generated, and the connection never set the mandatory UTC session zone.

## Decisions

### 1. The evaluator owns composition only

Every answer that depends on how DuckDB **parses, casts, or renders a
value** goes through the single probe-pinned owner in
`compare.rs`/`conform.rs`. `eval.rs` composes — arity, null propagation,
three-valued logic — and may not define a value domain; a new domain lands
in the probe matrix before it has a caller. Concretely:
`eval::parse_timestamp` is deleted and scalar string→TIMESTAMP coercion
delegates to `compare::literal_timestamp` (`ZoneRule::Ignore` — the exact
`TRY_CAST(text AS TIMESTAMP)` a bound parameter gets). The other domains
are untouched: catalog conform keeps `ZoneRule::Apply`, `strptime` keeps
its format-driven parse, the search stage keeps its door. Rejected:
narrowing both lanes to a shared kernel (a permanent feature tax to avoid
a test matrix the repo already builds well).

### 2. A failed coercion is NULL — the lexical fallback is withdrawn

This supersedes ADR-0001's clause prescribing lexical string ordering when
timestamp coercion fails. The fallback invents an ordering DuckDB does not
have, inverts under `NOT`, and makes `x > y` depend on whether the other
operand happened to parse. NULL/UNKNOWN is already the harness's ratified
rule ("eval nulls where batch errors"); the fallback was the exception
nobody reconciled.

### 3. `now()` is one instant per unit of output

- **Batch:** one instant per statement, captured in Rust and **bound as a
  TIMESTAMP parameter** — the SQL prefix and the `rust_stages` tail read
  the same value by construction (this also fixes the naive-vs-TIMESTAMPTZ
  type split of emitting bare `now()`). Supersedes ADR-0001's `now()`
  sentence.
- **Live pass-through:** one instant per event, sampled once into the
  evaluation context — a subscription's clock advances between rows while
  each row is internally frozen. (Per-tick was rejected: a bus batch is an
  upstream client's POST size, not a boundary the query author can see.)
- **Aggregate snapshots:** one instant per emitted snapshot for all its
  post-stage rows; pre-stage source events sample per event.

### 4. Deterministic drifts fix toward DuckDB — no third semantics

Int/Int `/` becomes true division (the documented `status / 100` example
changes); arithmetic overflow yields NULL, never a panic; `if()`/`case`
conditions take DuckDB's boolean domain, not truthiness;
`tonumber(true)` = 1.0; `ceil`/`floor` return types follow DuckDB
(probed); short-circuit evaluation of `if`/`case`/`coalesce` is probed
first and changed only if observably different. ADR-0001's accepted chrono
residuals (`%y`, `%z`, locale codes) survive, each as a pinned named test.

### 5. The harness is the instrument, and it fails loud

Infrastructure failure panics; a parse/emit rejection of generated grammar
fails; the only legal Skip is an unreadable value shape, counted per
category with a ceiling. Lenient matchers are deleted (type-exact
comparison through an explicit allowed-widening table; no whole-second
collapse). The connection runs `SESSION_TIME_ZONE_SQL`. Missing families
are added (operators, negatives, null-first `coalesce`, `%f`,
quarter/week, hostile timestamp corpus, `json_*`, `tonumber`/`tostring`
over every variant, `now()`). Every known divergence lands first as a
named test asserting **today's** behavior, so the fix PRs flip assertions
visibly. Consequence line: the probe matrix is release-gating on a bundled
DuckDB bump.

## Slicing

Three implementation issues, in order: (1) hostile harness + this ADR's
pinned-divergence tests; (2) value-domain fixes (§1, §2, §4); (3) `now()`
anchor plumbing (§3). `sev()` is out of scope (already one kernel).

## Amendment (2026-11-08): absolute search bounds go through `literal_timestamp`, and a bound it cannot read is refused

This amendment records the untangle prep of 2026-11-08 (defects D7 and D12). §1 said "the search stage keeps its door". That door was a second, unprobed timestamp parser, and the two lanes read one bound two ways:

- The live filter parsed `earliest=` and `latest=` with chrono's RFC 3339 parser and `.ok()` (`crates/trawl-core/src/filter.rs:212-222`). A spelling that `CAST(… AS TIMESTAMP)` accepts and RFC 3339 does not, such as `earliest="2026-03-14"` or a space separator, dropped the bound. The live stream then matched every event on that side.
- The live filter applied a bound's offset and converted it to UTC. The SQL emitter binds the written text and casts it with `CAST(? AS TIMESTAMP)` (`crates/trawl-core/src/emitter/search.rs:44-57`), which discards the offset. `earliest="2026-03-14T09:00:00+05:30"` was 03:30 live and 09:00 in batch.

**The search stage has no door of its own.** §1's sentence "the search stage keeps its door" is withdrawn. The new rule:

- Every `earliest=` and `latest=` clause is read once through `compare::literal_timestamp` (`ZoneRule::Ignore`, `crates/trawl-core/src/compare.rs:410-422`), the probe-pinned mirror of the cast a bound string gets. That reading is the accepted domain: date-only, `T` or space separator, a fraction, `infinity` and `-infinity`, and an offset that is read and then discarded.
- Both lanes discard a bound's offset and keep its wall clock, as ADR-0001 says for literals. The live lane changes. Batch does not.
- A clause that `literal_timestamp` cannot read or represent is refused before either lane compiles, with a span over that clause. No lane drops it. The refusal covers every clause in the text, including one that a later clause of the same kind overrides. A typo in an overridden clause is still a typo. For valid clauses, the last clause of each kind still wins (ADR-0061).
- The query AST keeps the bound as written, because the formatter prints it verbatim and ADR-0061's canonical form compares written slots. The parsed `compare::Instant` is what executes. The live filter compares `Instant`s: `earliest` is inclusive, `latest` exclusive, and the infinities order by `Instant`'s `Ord`. The emitter binds `Instant::cast_text()` (`compare.rs:343-350`), not the written text, so DuckDB reads canonical text and both lanes consume one value.
- An inverted interval (`earliest` after `latest`) stays an empty answer in both lanes.
- An unquoted `earliest=` or `latest=` stays a field filter (ADR-0062). Only parsed time clauses are checked.

`last=` combined with `earliest=` or `latest=` is refused in both lanes; ADR-0061's amendment of the same date records that rule.

**Stored query text may stop parsing on upgrade.** A saved query, schedule, investigation step or captured result whose DSL holds an unreadable bound fails its next validation or run with the diagnostic. Batch already failed on most of those texts at DuckDB's cast. A scheduled attempt goes through admission (`crates/trawl-server/src/scheduler.rs:386-398`) and records a refusal as a failed run (`:473-480`). The schedule keeps its row, and startup and other schedules continue. Restoring the old reading means restoring the consistent pair named in ADR-0061: the Postgres database and `[server] results_dir`, taken together with trawld stopped.

Considered, not adopted:

- **Refuse a bound that spells a non-zero offset.** Honoring or refusing offsets in literals is a separate decision (ADR-0051). Both lanes now agree on the wall clock, which is what ADR-0001 and this ADR already say.
- **Check only the effective bound after hoisting.** It keeps `earliest="bad" earliest="2026-03-14"` legal. Refusing the overridden typo costs nothing a forward-only, pre-1.0 product must keep.
- **Keep the RFC 3339 door and widen it to DuckDB's set.** It would be a second parser to keep in step with the probe matrix, which is the class §1 removed from `eval.rs`.
- **Cache the parsed instant beside the text and let the emitter keep binding the text.** Two parsers would remain on the execution path, and a residual difference between them would change SQL without a test noticing.

Premise checks at `b71f9e6e`: `filter.rs:212-222` (RFC 3339, `.ok()`, `with_timezone(Utc)`); `emitter/search.rs:44-57` (`CAST`, not `TRY_CAST`, over the bound text); `compare.rs:322-350` (`Instant` with ordered infinities and `cast_text`); `parser/search.rs:540-565` (each clause overwrites its slot, and `hoisted` keeps every clause's kind and span); `format.rs:450` prints the written text, and `canonical_form.rs:139-158` compares the written slots.
