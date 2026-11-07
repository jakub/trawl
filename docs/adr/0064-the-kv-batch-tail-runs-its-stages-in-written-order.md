# The kv batch tail runs its stages in written order

status: accepted (2026-11-06), prep record for the kv batch tail order fix (#346); human rulings of 2026-11-06 recorded below

`extract kv` mints columns that SQL cannot name, so the emitter sends it and every later stage to the kv batch tail (`emitter/mod.rs:490-503`). The tail runs over the rows the SQL prefix returned. On main (`42e05d0f`) it does not run those stages in the order the query wrote them:

- `post_process` takes every `sort` out of the pipeline (`post_process.rs:101-104`), runs the rest through `compile_stream_plan_with`, and applies the sorts at the end (`post_process.rs:152`). `* | extract kv | sort -bytes | head 2` runs as `head 2 | sort -bytes`. With input bytes 10, 20 and 100 it answers 20, 10 instead of 100, 20. `stats count() by method | sort -count | limit 5` keeps five groups in hash-map order (`stream.rs:2401-2418`) and only then sorts them. Neither query raises an error.
- A descending sort reverses the whole comparison (`post_process.rs:565`), NULL placement included, so NULL and absent cells sort first. Batch SQL puts them last in both directions, which is DuckDB's default.
- A sort key is looked up by its exact name (`post_process.rs:562`). Every other stage in the tail binds a name the way DuckDB binds an identifier (`eval.rs:457`).
- `dedup` with fields keeps the first row it sees for a key and clears its key set at 10,000 keys (`stream.rs:1378`, `:1744`). The reference promises the row with the latest `_time` (`dsl.md:760`), and batch SQL keeps that row (`emitter/pipeline.rs:397-409`). Past 10,000 keys the tail returns a key twice.
- `tail` does nothing (`stream.rs:1672`). Its comment says the buffer handles it, and the kv tail has no buffer. `extract kv | tail 5` returns every row.

The live lane refuses `sort` (`stream.rs:283`). It evicts dedup keys because a subscription has no end. Neither reason applies to a batch answer, which has every row before the first stage runs.

## Decision

**Stages run in the order the query writes them.** The kv batch tail compiles its stages into an ordered list of steps. A step is either a run of streamable stages, compiled as today into a `StreamPlan` (pass-through or aggregate), or a barrier. A barrier reads its whole input before the next step sees a row. `sort` and `dedup` with fields are barriers. Each step takes the previous step's rows by value and hands its own rows to the next. `* | extract kv | sort -bytes | head 2` sorts, then takes two rows.

**The whole tail is validated once, before any row runs.** The complexity check, the projection collision check, the unsupported-stage refusals and the one-aggregation rule cover every stage of the tail, not each step alone. A barrier does not reset the one-aggregation rule: `stats … | sort … | stats …` is refused as before. Every step compiles before the empty-schema return (`post_process.rs:121`), so a refusal later in the tail still fires when the SQL result has no columns.

**Steps share one statement context.** One pin scope walk advances across all steps, and a `sort` leaves it unchanged (`pin_scope.rs:165`). All steps share the statement anchor (ADR-0017 §3), the `last` span that a `timechart` reads, one pattern ledger (ADR-0063) and one cancel latch. Splitting the tail into steps changes no value a stage computes.

**Each step keeps the stage protocols.**

- `Done` from a `limit` ends its own run. The rows that run kept go to the next step. `head 0` produces no rows and the later steps still run over none.
- An aggregation that drops input at a cap refuses with `EngineError::AggregationCapped` when it finishes, before any later step runs. A later `head` never hides it. ADR-0001's batch rule (`0001-streaming-eval-duckdb-parity.md:116`) stays as written.
- The tail reads the cancel latch between steps, and a barrier reads it as `sort_rows` does today (ADR-0063).
- Memory grows with the rows: a step holds its input and its output, and no step copies the whole result twice. The SQL read under `RowCap::Refuse(tail_input)` still bounds the input, and `cap.trim` still cuts the answer after the tail (`executor.rs:2624`).

**A sort is stable and puts NULLs last in both directions.** A sort key binds per row through `bind_event_key`: the exact name first, then the lowest ASCII-insensitive match. A row without the key reads it as NULL. A NULL sorts after every value, ascending or descending. A descending sort reverses only the comparison of two values. Numbers keep the exact order `compare_cells` gives them today, NaN and large `u64` included. Rows with equal keys keep their input order. Several sorts apply in order, so the last one is the primary key and the earlier ones break its ties.

**Dedup in the kv batch tail is exact and keeps the newest row per key (human ruling, 2026-11-06).** No key set is cleared. `dedup` with fields keeps, for each key, the row with the latest `_time`, as the reference and batch SQL do. The tail reads `_time` the way `TRY_CAST(_time AS TIMESTAMP)` does. `compare::literal_timestamp` already mirrors that cast. A row whose `_time` is absent, NULL or unreadable ranks after every row with a readable time. Among rows with the same time, the earliest in the step's input wins. The step returns the winning rows in their input order. Key identity does not change: the key is built by `dedup_key`, so an absent field and a NULL are one key. Bare `dedup` keeps the first of each set of identical rows, with no reset. Rows that are identical have the same `_time`, so "first" and "newest" choose the same row. The rule applies wherever `dedup` sits in the tail, after a `sort` or an aggregation too. After an aggregation that drops `_time`, every row ties, so the first row of each key wins.

**`tail` is refused in the kv batch tail until it has its own contract (human ruling, 2026-11-06).** `extract kv | … | tail N` fails validation with `EngineError::Emit(EmitError::UnsupportedOperation)`, a 400 whose sentence names `tail` and `extract kv`. In batch SQL, `tail` means "order by `_time` descending first, unless an earlier `sort` set the order" (`dsl.md:602`). The tail has no settled reading of that rule after a rename, an aggregation or a dropped `_time`. A later issue defines it.

**The live lane does not change.** It still refuses `sort`, keeps the first-seen dedup row with its 10,000-key reset, and treats `tail` as it does today.

**The reference changes with the code.** The pull request that implements this decision updates `dsl.md`: the lane table (`sort`, `dedup`, `tail`, and the one-aggregation rule that the kv batch tail already applies), the `sort` section (NULLs last in both directions), the `dedup` section (newest per key, exact), and the `tail` section (refused after `extract kv`).

Considered and rejected:

- **Refuse a sort that has an order-sensitive stage after it.** The issue offered this. It would refuse `stats count() by method | sort -count | limit 5`, the most common top-N query after `extract kv`.
- **Use batch SQL as the oracle for every shape.** The SQL lane appears to have the mirror defect: `head 5 | sort x` with no projection emits `ORDER BY x LIMIT 5` (`emitter/pipeline.rs:137`, `emitter/state.rs:1052`, `:1233-1240`). Tests compare against SQL only for `sort | head` and `stats | sort | limit`, and use expected rows computed by hand elsewhere.
- **Fix dedup in a separate issue.** Human ruling: fix it here, because dedup becomes a barrier in the same change.
- **Keep first-seen dedup and document it as a lane difference.** Human ruling: the reference and batch SQL already promise the newest row, so the tail follows them.
- **Implement `tail` now.** Human ruling: refuse it until its rule after a rename, an aggregation or a dropped `_time` is decided.
- **Promise one vector of rows for the whole tail.** A barrier needs its input and its output at once, so the claim would be false. The decision promises memory that grows with the rows instead.

Out of scope, and not filed by this prep: the batch SQL lane's `head N | sort x` order. It was traced, not run.
