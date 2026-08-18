# Scalar statement semantics: one mirror per value domain, one instant per unit of output

status: accepted (2026-08-18) — prep ruling record for #93

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
