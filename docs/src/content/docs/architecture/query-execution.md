---
title: Query execution
description: Understand how one query reads recent and compacted events and returns one answer.
---

Why does one query give one answer, whether its events are still in memory or already in Parquet? A query takes one catalog pin snapshot at the start and carries it to the end. Recent events read through the same conformance expressions that wrote the Parquet files, so both halves of the union agree on what a value means. Embedded `--data` queries have no catalog and stay pin-blind.

## Parser and SQL emitter

The parser owns token boundaries and comments. The emitter builds CTEs, binds values, validates projection names, and caps expression expansion before DuckDB sees anything. A stage is not always one CTE, because the emitter accumulates compatible operations before it flushes.

`PinScope` follows a field through the pipeline. A rename and a bare alias keep the pin, and a computed replacement loses it. Grouping keys keep their scope, while aggregate outputs are new values. `from saved` clears catalog typing, because a report's stored columns are not event fields.

A bare field compared against a literal uses the shared rule table. The [DSL reference](/reference/dsl/#pinned-comparison-semantics) holds the complete operator rules.

## Source computation

The planner narrows files by environment, service, and time across hourly and daily paths. The executor then resolves each list element once, after it takes the hot snapshot. Literal paths use filesystem metadata and patterns use DuckDB globbing. `read_parquet` rejects a whole list when one element matches nothing, so a sparse service needs this step. The evidence it collects also drives error handling, so a file that matched and then vanished is never dropped in silence.

A publication read guard protects source selection through the last physical read, and compaction takes the matching write guard around publication and hot drain. That is what stops one event appearing in both union branches. See [ingest and publication](/architecture/data-flow/#hot-buffer-and-the-publication-guard) for the full lock order.

## Hot buffer integration

A cached snapshot file represents the current hot generation. If the snapshot cannot be built, the query is refused with 503 rather than answered from Parquet alone. It puts every observed field name inside DuckDB's schema-detection prefix, and carries the current pins intersected with its own keys. It also records which keys held only `null` in that generation. On the hot branch, such a key reads as a typed NULL: its pin type, or the cold type when cold files carry it. An unpinned null-only key that no cold file carries is left out, as compaction leaves it out of Parquet.

The emitter conforms the hot branch and leaves conformant cold files alone. See [hot and cold agreement](/architecture/catalog/#hot-and-cold-agreement).

## Input columns

A field the input does not carry reads as NULL ([DSL reference](/reference/dsl/#missing-fields-and-nulls)). For that, the emitter must know which columns the input has. After source resolution and the hot snapshot, the executor runs one `DESCRIBE` over the exact readers the query will scan. A `from saved` source is described from its own reader. A recorded run is typed by the columns it stores, so on that lane, and on a capture of one, a live pin types only a name the run carries, and a field the run lacks reads as an unpinned NULL. The probe never reads a fresh glob or the catalog, because the catalog proves a type, not presence.

There is one probe per request, whatever the number of absent names. It runs inside the query's permit, deadline and cancel latch, and counts as `probe` work in [query timing](/architecture/reports-telemetry/), never as a bind. On the lanes that read the live corpus (query, export and window captures), the publication read guard covers source resolution, the probe and the execution together. `from saved` and a capture of a recorded run take no publication guard (ADR-0024). Nothing adds a file to a recorded run, so the probe and the execution see the same files. Retention can still delete them mid-request, and then the read fails rather than answering NULL. Embedded `--data` has no such guard, so its files must not change while a query runs: a file added between the probe and the execution reads a column the probe did not see as NULL. The executor skips it when the query reads only `_time` and `_ingested`, with no text search and no extract of the default `message`. A hot snapshot with an unpinned null-only key needs the probe anyway.

The emitter then tracks the columns through every stage, beside `PinScope`. At each read of a name the current relation lacks, it writes `CAST(NULL AS <pin type>)` for a pinned field and an untyped `NULL` otherwise. A few function arguments are the exception: an untyped `NULL` leaves DuckDB no single overload for the timestamp of `date_part`, `date_trunc` and `date_diff`, the operand of a percentile, or the arguments of `contains`, so an argument there that is NULL by construction is a NULL of the type that call needs. That is an unpinned absent field, or a `coalesce`, `if` or `case` of nothing else. After a dynamic `pivot`, names read downstream come from a zero-row `UNION ALL BY NAME` branch on the pivot's CTE, so the pivot needs no `DESCRIBE`. The branch's NULLs have the type of the pivot's aggregate, so a value column that no event produced has the type of one that an event produced.

## Errors and fallback

Query and Parquet-export entry points share one outcome-policy table. Hot-only fallback serves a cold start only, where no cold data exists to hide. Any other failure with cold data present is an error.

A column that DuckDB still cannot find after resolution is a resolver defect. Every lane answers it with a 400 `unknown field` that names no SQL, path or DuckDB text, and never retries it against less data.

A source without `_time` or `_ingested` is foreign or broken. A read of either from such a source is a 400 that names the column. One older exception remains: `* | timechart span=1h count()` over a source without `_time` fails in DuckDB and returns a database error instead.

A no-files read, after source evidence established that cold data exists, returns a retryable `503 cold_data_unread`, not an empty success. A genuinely empty source returns zero rows, and an empty Parquet export reports its underlying error. A type conflict outside the catalog contract stays an error, because read-time reconciliation is not a fallback.

## Executor pool and deadlines

The pool bounds concurrent physical DuckDB work. Request lifetime and worker lifetime differ, because DuckDB binding can continue after an interrupt or a timeout. A timed-out request therefore keeps its permit and its publication guard while the work runs. The running-query listing exposes it, and a repin cutover waits for real exclusion rather than for HTTP responses to finish.

Every query, export, and field sample runs on one query database with a spill cap, a memory limit, and a thread count, and spills into the query scratch directory. A query that goes over a budget fails with 503 `query_resource_exhausted`. The memory and spill budgets are database-global, so concurrent queries share them, and one heavy query can make another fail with the same code. Regex work has its own memory bounds: a per-query budget and a 1.25 GiB reservation that live streams and batch queries share. DuckDB's RE2 counts in that reservation, and a query that does not fit gets the same 503 with a regex cause. The engine sample shares the query database's spill, memory and thread budgets too, and holds no regex memory. A resource failure in the sample reads `slow`, which is a degraded 200, while a sample has succeeded in the last 60 seconds. After 60 seconds with no success it reads `error`, which is a 503. The health check does not take a permit: it reads the latest engine sample, which a separate thread takes every 10 seconds, so a full pool never fails health. See [query scratch and budgets](/reference/configuration/#query-scratch-and-budgets) and [the engine sample](/operate/health/#the-engine-sample).

## Preparation off the reactor

Preparing query text means parsing it, validating it and building its patterns, and near-limit patterns can spend about 0.85 s of CPU on that. No door prepares on a reactor thread, so a heavy query never stalls health, metrics or other requests. Each door runs its preparation as one job on the blocking pool, behind the preparation allowance (ADR-0063). The code names four doors:

- **Admission**: the parse and validation for `/query`, export, saved-query create and update, and the scheduler.
- **Validate**: `/api/v1/validate`, whose parse, validation and formatting are one job.
- **Stream**: SSE setup, which is admission and the live compile in one job.
- **Capture**: capture planning, which is admission and the bounds in one job. Compare, sightings and context reach it through the capture service, and compare composes its statement in a capture job of its own first.

The allowance has half the cores, at least 2 and at most the executor permits, so with one permit it is one slot. A job waits for a slot within its request's deadline. If no slot comes free in time, the request gets 503 `query_resource_exhausted` with the cause `preparation_allowance_exhausted`, and nothing ran. A preparation job never holds or waits for an executor permit. The request limit (ADR-0054) still applies first.

A slot belongs to the job, not the request. When the request stops waiting, at its deadline or because its caller went away, it asks the job to stop. The job ends at its next cancellation point and keeps its slot and its regex reservation until its closure exits.

A deadline that passes while a job runs answers 504 `timeout` for `/query`, export, `/validate` and SSE setup, and for compare's own composition job. Capture planning keeps its earlier answer: a deadline that passes before the capture is admitted answers 503 `server at capacity: the query was not started`, booked as `capacity_refused`. Compare, sightings and context answer the same way from capture planning.

A cancelled preparation is never a diagnostic. It is never a parse error, a 400 or a `valid: false`.

The window checks that run inside a store transaction, for a saved query or a schedule, parse without building patterns. They run on the blocking pool with no slot, so a row lock never waits for a slot.

## Rust tail and display

`extract kv` splits execution into SQL plus a Rust tail. The tail evaluates the later stages and receives the pin scope at the split, so it does more than formatting. Display offsets apply after it, which keeps evaluation on UTC instants. Severity columns render as tokens in human-readable views and as numbers in machine formats. See [severity syntax](/reference/dsl/#severity-_severity).

## SSE streaming

A live stream filters event-bus batches in Rust instead of querying DuckDB per event. Its filter and its stages share one pin snapshot for the stream's lifetime, so a repin reaches an existing stream only after you reconnect. The matcher follows the shared comparison rules and SQL three-valued logic.

Matching runs regexes, so it runs off the reactor too. The stream's driver waits for the bus on the reactor and hands events to the blocking pool in chunks, with one job at a time for each stream. A chunk takes only the events that have already arrived, up to 256 (`LIVE_CHUNK_EVENTS`, calibrated in `docs/evidence/issue-336/live-chunk.txt`) and up to 256 KiB of string payload. The cap bounds batching, not event size: an event cannot be split, so one event over the cap runs alone. A stage can make an event's output far larger than its input, so a job also stops taking events once its frames reach 1 MiB (`LIVE_JOB_FRAME_BYTES`), and the events it did not take run in the next job. While a job runs, the driver receives nothing, and the bus's ring is the bounded queue in front of the stream. A stream that goes away mid-job stops at its next event or regex call. Its state drops inside the job, which then releases the stream's SSE slot and regex reservation.

Not every stage can stream, and the server refuses the ones that cannot. See [batch and streaming differences](/reference/dsl/#batch-and-streaming-differences). The event bus is bounded and lossy, so a lag notification means events were missed with no durable backlog to replay.
