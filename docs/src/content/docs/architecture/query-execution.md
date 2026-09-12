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

A cached snapshot file represents the current hot generation. It puts every observed field name inside DuckDB's schema-detection prefix, and carries the current pins intersected with its own keys, so the SQL never names an absent hot column.

The emitter conforms the hot branch and leaves conformant cold files alone. See [hot and cold agreement](/architecture/catalog/#hot-and-cold-agreement).

## Errors and fallback

Query and Parquet-export entry points share one outcome-policy table. Hot-only fallback is allowed only where it cannot hide cold data, such as a cold start or a permitted missing column. Any other failure with cold data present is an error.

A no-files read, after source evidence established that cold data exists, returns a retryable `503 cold_data_unread`, not an empty success. A genuinely empty source returns zero rows, and an empty Parquet export reports its underlying error. A type conflict outside the catalog contract stays an error, because read-time reconciliation is not a fallback.

## Executor pool and deadlines

The pool bounds concurrent physical DuckDB work. Request lifetime and worker lifetime differ, because DuckDB binding can continue after an interrupt or a timeout. A timed-out request therefore keeps its permit and its publication guard while the work runs. The running-query listing exposes it, and a repin cutover waits for real exclusion rather than for HTTP responses to finish.

## Rust tail and display

`extract kv` splits execution into SQL plus a Rust tail. The tail evaluates the later stages and receives the pin scope at the split, so it does more than formatting. Display offsets apply after it, which keeps evaluation on UTC instants. Severity columns render as tokens in human-readable views and as numbers in machine formats. See [severity syntax](/reference/dsl/#severity-_severity).

## SSE streaming

A live stream filters event-bus batches in Rust instead of querying DuckDB per event. Its filter and its stages share one pin snapshot for the stream's lifetime, so a repin reaches an existing stream only after you reconnect. The matcher follows the shared comparison rules and SQL three-valued logic.

Not every stage can stream, and the server refuses the ones that cannot. See [batch and streaming differences](/reference/dsl/#batch-and-streaming-differences). The event bus is bounded and lossy, so a lag notification means events were missed with no durable backlog to replay.
