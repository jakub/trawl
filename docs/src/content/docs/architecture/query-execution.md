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

There is one probe per request, whatever the number of absent names. It runs inside the query's permit, deadline, cancel latch and publication guard, and counts as `probe` work in [query timing](/architecture/reports-telemetry/), never as a bind. The executor skips it when the query reads only `_time` and `_ingested`, with no text search and no extract of the default `message`. A hot snapshot with an unpinned null-only key needs the probe anyway.

The emitter then tracks the columns through every stage, beside `PinScope`. At each read of a name the current relation lacks, it writes `CAST(NULL AS <pin type>)` for a pinned field and an untyped `NULL` otherwise. A few function arguments are the exception: an untyped `NULL` leaves DuckDB no single overload for the timestamp of `date_part`, `date_trunc` and `date_diff`, the operand of a percentile, or the arguments of `contains`, so an unpinned absent field there is a NULL of the type that call needs. After a dynamic `pivot`, names read downstream come from a zero-row `UNION ALL BY NAME` branch on the pivot's CTE, so the pivot needs no `DESCRIBE`.

## Errors and fallback

Query and Parquet-export entry points share one outcome-policy table. Hot-only fallback serves a cold start only, where no cold data exists to hide. Any other failure with cold data present is an error.

A column that DuckDB still cannot find after resolution is a resolver defect. Every lane answers it with a 400 `unknown field` that names no SQL, path or DuckDB text, and never retries it against less data.

A source without `_time` or `_ingested` is foreign or broken. A read of either from such a source is a 400 that names the column. One older exception remains: `* | timechart span=1h count()` over a source without `_time` fails in DuckDB and returns a database error instead.

A no-files read, after source evidence established that cold data exists, returns a retryable `503 cold_data_unread`, not an empty success. A genuinely empty source returns zero rows, and an empty Parquet export reports its underlying error. A type conflict outside the catalog contract stays an error, because read-time reconciliation is not a fallback.

## Executor pool and deadlines

The pool bounds concurrent physical DuckDB work. Request lifetime and worker lifetime differ, because DuckDB binding can continue after an interrupt or a timeout. A timed-out request therefore keeps its permit and its publication guard while the work runs. The running-query listing exposes it, and a repin cutover waits for real exclusion rather than for HTTP responses to finish.

Every query, export, and field sample runs on one query database with a spill cap, a memory limit, and a thread count, and spills into the query scratch directory. A query that goes over a budget fails with 503 `query_resource_exhausted`. The memory and spill budgets are database-global, so concurrent queries share them, and one heavy query can make another fail with the same code. The engine sample shares them too. A resource failure in the sample reads `slow`, which is a degraded 200, while a sample has succeeded in the last 60 seconds. After 60 seconds with no success it reads `error`, which is a 503. The health check does not take a permit: it reads the latest engine sample, which a separate thread takes every 10 seconds, so a full pool never fails health. See [query scratch and budgets](/reference/configuration/#query-scratch-and-budgets) and [the engine sample](/operate/health/#the-engine-sample).

## Rust tail and display

`extract kv` splits execution into SQL plus a Rust tail. The tail evaluates the later stages and receives the pin scope at the split, so it does more than formatting. Display offsets apply after it, which keeps evaluation on UTC instants. Severity columns render as tokens in human-readable views and as numbers in machine formats. See [severity syntax](/reference/dsl/#severity-_severity).

## SSE streaming

A live stream filters event-bus batches in Rust instead of querying DuckDB per event. Its filter and its stages share one pin snapshot for the stream's lifetime, so a repin reaches an existing stream only after you reconnect. The matcher follows the shared comparison rules and SQL three-valued logic.

Not every stage can stream, and the server refuses the ones that cannot. See [batch and streaming differences](/reference/dsl/#batch-and-streaming-differences). The event bus is bounded and lossy, so a lag notification means events were missed with no durable backlog to replay.
