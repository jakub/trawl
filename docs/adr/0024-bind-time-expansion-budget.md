# A static lateral-expansion budget bounds bind-time cost, and the query permit deadline is made honest

status: accepted (2026-09-05) — prep ruling record for #150; last open checkbox of the #98 sweep

A chained `let` stage where each assignment references the previous one
twice (`a1 = a0 + a0, a2 = a1 + a1, ...`) emits SQL that is linear in the
number of assignments, but within one stage a target the row does not
carry is a DuckDB lateral alias: the binder substitutes the alias's bound
expression tree per reference, so the bound tree doubles per assignment.
O(N) DSL text becomes O(2^N) bind-time work, spent inside `conn.prepare()`
before a row is read. Reproduced on this workspace's DuckDB 1.5.5: a
266-byte query wedges a bind past 30 seconds. `duckdb_interrupt()` is a
no-op during bind (an interrupt 100ms into a 4s prepare returned only
after the bind finished), the ExecutorPool permit rides the whole
uninterruptible bind, and the query timeout frees only the HTTP caller,
so enough such queries starve the pool and the repin cutover's
`exclusive()` acquisition. `MAX_EXPR_DEPTH` does not touch this: each
`b = a + a` is one shallow expression, and the assignment count is bounded
only by the 64 KiB `MAX_QUERY_LEN`.

## Rulings

1. **The bound is a static lateral-expansion budget, not a raw node
   count.** For each `let`/`eval` stage, walk assignments left to right,
   memoizing each target's substituted weight (a reference to an earlier
   same-stage target costs that target's weight, with no `+1`, because
   DuckDB substitutes exactly its tree). Expansion is
   `substituted_weight - source_node_count` summed across the query.
   Refuse when it exceeds `MAX_LATERAL_EXPANSION = 512`. A flat `IN` list
   and independent assignments have expansion 0 and can never trip the
   cap, so a strict number costs legitimate queries nothing and the
   budget measures only genuine substitution. The rejected alternative
   was an undifferentiated expanded-node count: at any threshold it must
   sit above the largest legitimate flat query, which either admits
   seconds of binder CPU (32768, pinned to the text ceiling) or rejects a
   legal large `IN` list (8192). The delta dissolves that trade.

2. **512 targets binder latency, not the query timeout.** It caps the
   worst accepted bind near 0.3s of non-interruptible CPU on the
   reference host. Staying under the 30s query timeout was the wrong
   yardstick: the threat is a small authenticated request buying seconds
   of uninterruptible binder CPU on every executor, so the target is
   ordinary-planning latency. No legitimate query reaches a 512
   expansion; a reused subexpression scores in the single digits.

3. **The checker is adversarial and stays linear.** Memoize each
   target's weight as the stage is walked; never recursively expand an
   alias tree, which would reproduce the exponential work in Rust before
   rejecting it. Arithmetic saturates in `u64` so a deep chain reports
   over-limit instead of panicking on debug overflow.

4. **The verdict is a conservative upper bound, corpus-independent.**
   Whether `a0` resolves to a lateral alias or a real incoming column
   depends on the corpus, which the validator cannot authoritatively
   see, so an earlier same-stage target name is treated as lateral. A
   verdict that changed with ingest would make a query legal on one node
   and refused on its replica, the reasoning ADR-0011 uses taking
   `FieldCatalog::all()` over `intersect`. Names match under the ASCII
   fold (`catalog_key`), because DuckDB binds identifiers
   case-insensitively.

5. **Pipeline stage count is a separate axis: `MAX_PIPELINE_STAGES =
   128`.** Each emitted stage is a CTE carrying a `COLUMNS(c -> ...)`
   lambda over every corpus column, so per-stage bind cost scales with
   corpus width, which the query text cannot see (512 stages bind in
   ~618ms with near-zero expansion). A weight constant standing in for
   corpus width would be a fiction, so the count gets its own cap. 128 is
   about twenty times the longest pipeline in the repo and keeps the
   refusal in trawl's words, not DuckDB's version-dependent
   "Max expression depth limit of 1000".

6. **The limit is whole-DSL validity, gated at both doors.** Expansion
   and stage count are a static property of the AST, so a query is legal
   or not with the same answer in every lane. The check runs first in
   `emitter::validate_pipeline` (batch SQL, embedded, hot-only, export,
   `/validate`) and in `stream::compile_stream_plan` (new
   `StreamPlanError::TooComplex`), the projection.rs one-check-both-doors
   precedent. The SSE and post-`extract kv` Rust tails are provably
   O(N) (they memoize each `let` result as a scalar in the row map, so a
   reference reads a value, not the producing expression) and refuse
   anyway, for one contract: a query cannot stream and 400 in batch. The
   error message names the mechanism and the remedy, split the chain
   across separate `| let` stages, which the planner materializes instead
   of substituting, dropping the expansion to 0.

7. **Saved queries are refused at author time.** `POST`/`PUT /saved` run
   the same check, so a pathological saved query is refused to its author
   rather than discovered by the scheduler; a stored query that later
   exceeds the cap fails its run with the same sentence.

8. **The query deadline is one budget, taken in the handler.** The
   deadline is created before the pre-pool work (`from saved` resolves
   its source before the pool call) and covers handler pre-work, queue
   acquisition, and execution, so the client's worst case is one budget,
   not a queue wait plus a fresh execution timeout. No permit before the
   deadline is `503 Service Unavailable` (remedy: retry); a query that
   ran and overran keeps `Timeout` (remedy: simplify). `ping()` and
   `sample_field_values()` get short fixed acquisition budgets and 503 on
   exhaustion, so a health check and autocomplete do not hang behind
   retained binders.

9. **A retained permit is kept, but counted and cancellable.** The permit
   is not released at timeout: returning a connection DuckDB is still
   using would corrupt it and would break `exclusive()`, whose atomicity
   budget is that holding every permit implies nothing reads parquet.
   Instead the retained work enters a registry keyed by query id, owner,
   timeout instant, and duration; its interrupt handle stays reachable so
   `cancel_by_id` and `GET /queries` still reach it (the interrupt lands
   the instant bind ends and execution begins). `trawl_query_permits_retained`
   is the gauge to alert on, paired with `trawl_retention_suppressed`,
   because a repeatedly-blocked cutover is a growing archive. This
   narrows ADR-0011's "cost belongs inside the permit" rather than
   reopening it: the work inside a permit is now finite by construction,
   and the residual unkillable bind is measured instead of silent. Same
   shape as ADR-0021 rejection #4 from the other side, a throttle that
   was real but whose contract was wrong.

## Rejected

- **Rely on `duckdb_interrupt` or DuckDB's own `max_expression_depth`.**
  Interrupt is not honored during bind on 1.5.5; the 1000-expression
  backstop stopped a 1024-CTE chain but not the 16-alias exponential
  projection.
- **Release the permit at timeout, or kill the blocking thread.** The
  first corrupts the connection and breaks `exclusive()`; the second is
  not possible safely under `unsafe_code = "forbid"`, and process
  isolation for bind is out of proportion for a single-node product.
- **Make either limit configurable.** A knob invites raising it to "fix"
  a query, which restores the denial-of-service.
