---
title: Reports and telemetry
description: Understand why scheduled runs tile and what the daemon observes about itself.
---

Why can you add up a week of hourly reports and trust the total? A tiling schedule hands each run the interval that starts where the last successful run stopped. The runs never overlap and never leave a gap, so summing them counts every event once. The [schedules API](/reference/api/#schedules) holds the field rules.

## Scheduled reports

A saved query plus a schedule produces stored report runs. PostgreSQL holds the metadata, and Parquet report files live under `data/scheduled/`. A stored run is the original result, so running its DSL again can give a different answer.

### Why windows tile

`window = "since_last"` keeps a watermark, `covered_through`, at the end of the newest window a successful run covered. Each run covers from there to its own fire time. The watermark advances only on success, so a failed run leaves it standing and the next success covers both intervals in one window. That is what makes the runs add up. A fixed trailing window such as `window = "2h"` re-measures from every fire and heals nothing.

The scheduler fires on a planned cursor, not on time elapsed since the last run. A slow run, a late poll, and a restart leave the cursor where it was. When several boundaries have passed, the tick takes the latest one at or before now, so missed fires coalesce into one run.

Coalescing has a limit, because a week of downtime would otherwise produce one week-wide query. A gap wider than `max_catchup_intervals` clamps the window start forward, and the run records `window_truncated: true` and increments `trawl_scheduler_window_truncated_total`. A manual run of a `since_last` schedule is clamped and counted the same way, so the counter sees scheduled and manual runs alike. Alert on that counter. A truncated run leaves part of the catch-up outside any report, though those events stay in the corpus.

### Why lag delays coverage

Reports measure `_time`, the sender's own timestamp. That is what makes a report agree with an interactive query over the same interval, and it lets the partition layout prune the read. It also means an event can arrive after its window has closed.

`lag` is the allowance for that delay, and it shifts both bounds back rather than widening the window. With `lag = "5m"` the 03:00 run covers up to 02:55, so an event stamped 02:54 that arrived at 02:58 still lands in its window. Coverage arrives later and stays complete.

The [from saved stage](/reference/dsl/#from-saved) queries materialized runs instead of executing the saved query again. It clears the catalog pin scope, so a stored column that shares a name with an event field does not inherit its type.

## Internal telemetry

With internal telemetry enabled, a tracing layer turns daemon events into ordinary `service=trawld` records through the same [event contract](/reference/events/) as any producer. Payload fields cannot rename its service.

A flush writes its batch through the ingest WAL on Tokio's blocking pool, and only a successful write reaches the hot buffer and SSE. A failed write keeps its batch in a FIFO retry queue that drains oldest first, in coalesced writes of at most 4 MiB each. A cancelled write has already consumed its batch, so Trawl counts those events as lost.

A flush reserves hot-buffer space before it writes. Telemetry may fill the whole hot buffer, while HTTP and syslog stop at 15/16 of each cap, so trawld's records of an ingest stall can still land during it. If even that space is taken, the flush keeps its batch at the front of the queue and ends the cycle. It records no write failure and loses nothing. The next flush tries again after compaction drains. The batch waits under the same `telemetry_buffer_max_bytes` budget, and recording an event never blocks. A single event larger than a full hot-buffer cap can never be admitted, so Trawl drops it as a `buffer_cap` loss. See [hot-buffer admission](/architecture/data-flow/#hot-buffer-admission).

`telemetry_buffer_max_bytes` bounds one memory charge across the active buffer, the queued batches, and any in-flight write. At capacity Trawl sheds the oldest queued batches first, then new events. `trawl_telemetry_events_dropped_total` separates `buffer_cap`, `preinit_cap`, `write_crashed`, and `unmetered_cap`. `trawl_telemetry_bytes_dropped_total` covers only the first three, because an `unmetered_cap` drop counts events and no bytes. Missing internal events therefore never prove the daemon was idle.

### Query timing

`/metrics` shows that queries got slow. It cannot show where one query spent
its time. A query can wait for an executor permit, resolve its files, bind in
DuckDB, or execute, and each of those needs a different fix. So trawld writes
one `query_timing` event for each DSL query, and the event splits the query's
time into query phases. A DSL `stats` over those events then answers which
phase is slow, behind authentication, where a per-phase Prometheus histogram on
the unauthenticated `/metrics` would not. The recipes are in
[Find the slow phase of a query](/operate/health/#find-the-slow-phase-of-a-query),
and the decision is
[ADR-0046](https://github.com/jakub/trawl/blob/main/docs/adr/0046-a-query-reports-its-phases-in-one-timing-event.md).

Every DSL query that reaches its DSL check gets one account, refusals at that
check and capacity refusals included. `kind` is `query`, `from_saved`,
`export`, or `scheduled`, which covers manual runs too. Live tail, the pool
ping, and field sampling get none. Live tail never reaches DuckDB, and the
other two do not run user DSL.

Each phase is an integer number of microseconds in a field named
`query_<phase>_us`. Milliseconds would floor the short phases of a typical
query to zero. A phase the query never entered is absent, and a present zero is
a measured value. A phase entered more than once adds to one field. There are
14 phases:

| Phase | Covers |
| --- | --- |
| `dsl_check` | The entry point's parse and pipeline validation. |
| `saved_lookup` | `from saved` resolution, including its PostgreSQL read. |
| `pool_wait` | The wait for an executor permit. |
| `publication_wait` | The wait for the publication read guard. Absent for `from_saved`. |
| `startup` | Dispatch to the blocking pool, up to the moment work starts. |
| `source` | Source resolution, file discovery, and the pin snapshot. |
| `hot_snapshot` | The hot-buffer snapshot, including a build under its lock. |
| `emit` | The worker's parse and SQL emission, including fallback and debug-preview re-emits. |
| `probe` | Timechart input probes, both their bind and their execution. |
| `bind` | The DuckDB bind of the main statement. It ends when `prepare` returns. |
| `execute` | Statement execution up to the last row materialized, including parquet staging. |
| `copy` | The parquet `COPY` and its cleanup. |
| `post` | Rust tail stages, the cap trim, reorder, the severity walk, and the vanished-result guard. |
| `render` | Export body rendering, or the parquet readback. |

Each span of work counts in one phase only. A timechart probe's bind counts in
`probe`, never also in `bind` or `execute`. The bind inside a parquet `COPY` is
opaque to trawld, so the whole `COPY` counts in `copy`.

The rest of the event identifies the query and says how it ended. None of its
fields carries a user, a role, DSL, SQL, a literal, a path, or error text.

| Field | Value |
| --- | --- |
| `query_id` | The pool's query ID. It restarts at 0 each time trawld starts. |
| `kind`, `format` | The kind of work. `format` is `csv`, `json`, or `parquet`, on exports only. |
| `request_id` | The HTTP request's ULID, from its `X-Request-Id` header. Absent on report runs. |
| `run_id` | The report run's ID. Present on scheduled and manual runs only. |
| `outcome` | `success`, `error`, `capacity_refused`, `timeout`, or `abandoned`. |
| `work_started` | Whether the worker crossed its work-start transition. |
| `error_class` | The server's closed error class, on `outcome=error` only. |
| `timing_complete` | `false` when trawld wrote the event while work still ran. |
| `active_query_phase`, `active_elapsed_us` | On a partial event, the phase still running and how long it had run. |
| `duckdb_attempts` | How many times DuckDB bound the main statement. |
| `fallback` | Why there was more than one attempt: `none`, `raw_retry`, `hot_only`, or `both`. |
| `query_observed_us` | The whole window. It starts at the same instant as the response's `execution.duration_ms`, and for an export it runs through `render`. |
| `query_other_us` | The observed time no phase measured. Never negative. |

On a complete event, the present phases plus `query_other_us` add up exactly
to `query_observed_us`. On a partial event, the finished phases plus
`active_elapsed_us` plus `query_other_us` do. A large residual is a gap in the
measurement, not a phase to guess at.

A timeout or an abandoned request does not wait for its worker, because a bind
already inside DuckDB cannot be interrupted. The request writes a partial event
at once. When the work outlives its request, `query_permit_reclaimed` carries
the worker's final phase totals, `duckdb_attempts`, `fallback`, and a
`physical_outcome` of `completed`, `failed`, `cancelled`, `panicked`, or
`not_started`. The pool's registry lock decides which side writes the final
account, so each query gets exactly one. If the worker had already finished
and released its permit when the deadline fired, no reclaim is coming. The
request then writes a complete event with `outcome=timeout`.

A tracing event holds at most 32 fields. `query_timing` declares 31:
`event_type`, the 15 fields in the table above, 14 phases, and the message. A test
holds that count and requires one field per phase on both events, so a new
field on `query_timing` is a design decision, with one slot left.

Two older events changed with this one. trawld no longer writes
`pool_acquired`. Its one value, the permit wait, is now `query_pool_wait_us`,
which joins to the rest of the query. `scheduled_query_failed` carries
`error_class` instead of the error text, and the scheduled run events carry
`query_id`. The run record keeps the error text as its `error_message`.

### What telemetry does not cover

- Internal telemetry covers `trawld`. The proxy and the CLIs log to stdout and stderr for your collector.
- A coalescing poll observes Fleet key changes, so a key created and deleted between polls is invisible.
- Query lifecycle events carry identity, query ID, lengths, outcome, row counts, and timing, but not the raw DSL. Read that through authenticated history or an enabled query debug log.
- Metrics are Prometheus only, with no OTLP log or trace export.
- The targets `fleet_auth`, `auth.backend`, `preauth.transport`, and `trawl_server::policy::unmetered` never reach the WAL, whatever `RUST_LOG` says. Authentication runs before a per-key bucket can meter a request, so persisting those failures would let an unmetered client grow the corpus. `trawl_auth_failures_total{reason}` counts them instead.
- An unmetered server 5xx is the server's fault, not the caller's, so its `http_failure` event on `trawl_server::transport::failure::unmetered` does reach the WAL, under one process-wide cap of 60 events per minute. Only that exact target persists. A target beneath it never reaches the WAL. Events past the cap stay on stdout and count as `unmetered_cap` drops. [Trace a server failure](/operate/health/#trace-a-server-failure) lists the event's fields.
- The panic diagnostic on `trawl_server::panic` names the source location and never the panic message. It goes to stdout and `log_file`, never to the WAL. When the terminal monitor owns stdout and no `log_file` is written, or `RUST_LOG` filters the target out, trawld also writes the location line to stderr. The failure event of the caught request is the stored record.
