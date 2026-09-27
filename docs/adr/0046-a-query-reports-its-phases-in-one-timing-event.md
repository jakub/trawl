# A query reports its phases in one timing event

status: accepted (2026-09-27), prep record for slice A of query-path observability

`/metrics` shows that queries got slow, but nothing shows where a single query spent its time. The only split today is the permit wait in the opt-in query debug log. This record adds a `query_timing` event to trawld's own telemetry. The event splits each executed query into named **query phases**, so an operator finds the slow phase with the DSL. Per-operator DuckDB profiles are slice B and are out of scope here.

## Decision

**One root event per executed DSL query.** trawld emits `query_timing` once for each query that the executor pool runs: `kind` is `query`, `from_saved`, `export` (with a closed `format`: `csv`, `json`, `parquet`) or `scheduled` (manual runs included). Live tail, pool ping and field-value sampling emit nothing: live tail never reaches DuckDB, and the other two are not user DSL. The event is emitted with explicit fields from a `trawl_server` target, under the existing global filter and WAL layer. It does not rely on inherited span fields (ADR-0040), and it adds no span close hook, per-layer filter or exporter.

**Identity is explicit and survives a restart.** The event carries `query_id` and `kind`, plus `request_id` for HTTP work or `run_id` for scheduled and manual runs. `query_id` restarts at zero on every boot, so the ULID beside it is the durable key. There is no new attempt id. Scheduled run events gain `query_id` so the pool's retained-permit events join to a run.

**A closed outcome, no text.** `outcome` is `success`, `error`, `capacity_refused`, `timeout` or `abandoned`. `work_started` and `error_class` are added where they apply. The event carries no user, role, DSL, SQL, literal, path or error text. The outcome describes the query's execution only; a later scheduled-result write has its own events.

**Phases are integer microseconds.** Each phase is a flat field `query_<phase>_us`. Milliseconds would floor the parse, emit, bind and post phases of a typical query to zero. A phase the query never entered is absent; a present zero is a measured value. The phases are:

| phase | covers |
|---|---|
| `dsl_check` | entry-point parse and pipeline validation |
| `saved_lookup` | `from saved` resolution, including its postgres read |
| `pool_wait` | waiting for an executor permit |
| `publication_wait` | waiting for the publication read guard; absent for `from_saved` |
| `startup` | blocking-pool dispatch to the work-start transition (ADR-0024) |
| `source` | source resolution, file discovery, pin snapshot |
| `hot_snapshot` | taking the hot snapshot, including a build under its lock |
| `emit` | the worker's parse and SQL emission, including fallback and debug-preview re-emits |
| `probe` | timechart input probes, their bind and execution both |
| `bind` | DuckDB bind of the main statement |
| `execute` | statement execution and row pull, including parquet staging |
| `copy` | parquet `COPY` and its cleanup |
| `post` | Rust tail stages, cap trim, reorder, severity walk, vanished-result guard |
| `render` | export body rendering or parquet readback |

A phase entered more than once adds to the same field. `duckdb_attempts` counts main-statement binds, and a closed `fallback` (`none`, `raw_retry`, `hot_only`, `both`) says why there was more than one.

**The phases sit inside one observed window, with a visible residual.** `query_observed_us` runs from the same instant as the response execution record's `duration_ms` to the point the result is ready. For exports it extends through `render`. `query_other_us` is the observed time no phase measured, never negative. A large residual is a measurement gap to fix, not a phase to guess at. The response execution record (ADR-0027), `export_complete.duration_ms` and `trawl_query_duration_seconds` keep their meaning.

**A timeout reports at once, and the worker's final account comes later.** On timeout or abandonment, `query_timing` is emitted immediately with the completed phase totals, `timing_complete=false`, `active_query_phase` and `active_elapsed_us`. Elapsed time in an unfinished phase is never booked as a finished phase. If physical work outlives the request, `query_permit_reclaimed` carries the worker's final phase totals, `duckdb_attempts`, `fallback` and a closed `physical_outcome` when the permit comes back. The existing request and worker arbitration decides which side writes the final account, so it is written exactly once. A worker that never finishes gets no invented record.

**Trusted searches filter on the producer.** An HTTP sender can choose `service=trawld`; only the server stamps `_producer`. Documented recipes use `service=trawld _producer=trawld event_type=query_timing`.

**Field names are protected by prefix, not by contract.** Pins are keyed by field name alone (ADR-0009). The `query_` prefix makes a collision unlikely. If one happens, the operator repins. The names do not join the envelope.

**Visibility follows the existing telemetry boundary.** Any key that can search trawld's telemetry sees timing for every user's queries. The fields are numbers, closed enums and ids, and the lifecycle events already show more.

**Two adjacent cleanups ride this change.** `pool_acquired` is removed: its one datum is `query_pool_wait_us`, now joinable, and it cost one uncorrelated row per query. `scheduled_query_failed` stops persisting error display text and carries `error_class`, as ADR-0040 requires; the run record keeps the text. The opt-in query debug log's `timing_ms` uses the same phase map.

**Nothing new goes to Prometheus, and there is no knob.** `/metrics` is unauthenticated and the DSL answers per-phase aggregates. Timing events are best-effort telemetry under the ADR-0043 reserve and the telemetry buffer; drop counters tell an operator when rows are missing. Sampling would make that question harder to answer.

## Considered options

**Phase fields on the existing lifecycle events**, rejected: those events have different windows, and several paths emit none (a failed export, an abandoned request). One event gives one recipe and redefines no existing duration.

**A switch clock that charges every instant to the current phase**, rejected: uninstrumented time lands silently in whatever phase is current. The residual shows the gap instead.

**A new process-independent attempt id**, rejected: `request_id` and `run_id` already give every kind a restart-safe key. ADR-0040 relies on the request ULID for the same reason.

**A separate late-work event**, rejected: `query_permit_reclaimed` already fires from the same place at the same moment, for the same fact.

**Numeric contract pins for the phase names**, rejected: it widens the envelope and retypes other senders' fields. A telemetry sender also cannot refuse its own events, so a conflict would have no refusal point.

**Per-phase Prometheus histograms**, rejected for now: a DSL `stats` over the timing events answers the same question behind authentication.
