---
title: Ingest and publication
description: How durable events become queryable and move into Parquet.
---

## Ingestion pipeline

Every accepted event passes through the [event canonicalizer](/reference/events/). HTTP senders use bearer-authenticated ingest; syslog and internal telemetry supply profiles that assert the identity their transport can establish.

```text
HTTP / syslog / internal telemetry
          │
    canonical event
          │
     durable WAL write
          │
   hot buffer + event bus ──> live subscribers
          │
    hourly compaction
          │
      Parquet files ──> daily rollup
```

A successful write can leave the same event recorded in WAL and visible in the hot buffer. WAL is its durability record, not a mutually exclusive visibility state. Query execution combines recent and compacted data under a publication guard.

### WAL writer

A batch writes one file per environment/service group under `wal/{env}/`. Filenames include the validated service, arrival milliseconds, and a random suffix. Temporary-file publication and fsync barriers make successful writes durable before hot insertion. Names are validated rather than sanitized, so distinct legal service names remain distinct paths.

### Envelope canonicalization

The canonical [event reference](/reference/events/) defines the ten declared fields, optional values, sender namespaces, and repairs.

#### Producer profiles

Read [producer profiles](/reference/events/#producer-profiles) for HTTP, syslog, and telemetry identity precedence.

#### Syslog vocabulary

The listener publishes ordinary fields such as `syslog_severity`, `syslog_timestamp`, `syslog_facility`, `syslog_pid`, `syslog_msgid`, `syslog_source_ip`, and flattened `sd_*` fields. See [severity derivation](/reference/events/#severity-derivation) for the fixed syslog interpretation and [time derivation](/reference/events/#time-derivation) for yearless frame timestamps.

#### Timestamps

[Time derivation](/reference/events/#time-derivation) owns accepted encodings and the arrival-time fallback. Compaction's additional repair is described below.

### Hot buffer

The hot buffer holds shared batches until compaction drains them or capacity requires eviction. A generation-based snapshot cache lets queries share a temporary NDJSON file; pins for the snapshot's observed keys are refreshed independently of the cached file.

The publication lock prevents duplicate results during compaction. A query obtains its read guard before selecting files and keeps it through its last physical read. Compaction obtains the write guard for Parquet publication and hot-batch drain. Producers hold the corresponding ingest read guard from WAL writing through hot insertion, so a delayed producer cannot reinsert a batch after its drain.

Daily rollup holds publication exclusion through hourly-file retirement. An unfinished rollup refuses corpus reads until recovery finishes. These guarantees cover one daemon. They do not deduplicate client retries, crash-time WAL replay, or coordinate independent processes reading the same files.

### Event bus

A bounded Tokio broadcast channel shares batches with subscribers. A slow consumer can lag and receive a loss notification. It cannot block the producer, and the stream is not a durable replay queue.

## Compaction

The compactor finds eligible WAL files, groups them by service and environment, prepares a batch, and obtains catalog pins. It conforms the batch and combines it with the existing hourly file. Final publication and hot drain happen under the write guard, followed by bookkeeping and processed-WAL removal. Failed work retains the relevant inputs for diagnosis or retry.

### The field catalog: write-time type conformance

Read [catalog conformance](/architecture/catalog/#write-time-conformance) for guarded casts, type authority, and hot/cold agreement.

### Repin: the shadow-generation rewrite

Read [repin cutover](/architecture/recovery/#repin-cutover) for shadow layout, catch-up, exclusion, and force ceilings.

### Timestamp repair

Compaction reads each row's `_time` and `_ingested` with `TRY_CAST`. If a value is unusable, it uses the arrival instant recovered from that row's own WAL filename, then the compaction instant as the final fallback. The provenance is per row, not borrowed from the first file in a batch. Normal canonicalized events already have valid timestamps.

### Daily rollup

Hourly files consolidate into a daily file per service, ordered by `_time`. [Rollup recovery](/architecture/recovery/#daily-rollup) explains the marker and publication exclusion.

### Retention

Retention removes date directories inside individual environments. Age policy is per environment; disk-pressure selection ranks candidates across environments by the fraction of their age allowance consumed. An unlimited age policy does not exempt data from disk-pressure removal. Repin and epoch recovery can suppress deletion. See [recovery](/architecture/recovery/) and [configuration](/reference/configuration/) before changing policy.

## Storage layout

```text
data/
  EPOCH                         # current value: 3
  CATALOG                       # catalog identity proof when available
  prod/
    2026-09-10/
      00/nginx.parquet          # hourly output
      01/nginx.parquet
      nginx.parquet             # daily output after rollup
  scheduled/                    # materialized report results
```

The diagram shows both possible event-file shapes. Normal completed rollup retires its hourly inputs; queries must not count both copies. WAL may use its configured separate directory. Repin stages sibling roots, not subdirectories of this tree.

### The epoch cutover

The [epoch table](/architecture/recovery/#the-epoch-cutover) explains epoch 3 and both preserved old-root names.

### The boot conformance pass and the `CATALOG` marker

Read [boot conformance](/architecture/recovery/#boot-conformance-and-the-catalog-marker) for ownership, incomplete proof, and query-only behavior.

### App-state store

Trawl's PostgreSQL database stores history, saved queries, schedules, report metadata, and the catalog. Fleet authentication uses a separate keystore database. See [process boundaries](/architecture/overview/#components).

## Query execution

[Query execution](/architecture/query-execution/) explains batch SQL and Rust evaluation.

### Parser

See [parser and SQL emitter](/architecture/query-execution/#parser-and-sql-emitter).

### SQL emitter

See [parser and SQL emitter](/architecture/query-execution/#parser-and-sql-emitter) and the [DSL reference](/reference/dsl/).

### Executor pool

See [executor pool and deadlines](/architecture/query-execution/#executor-pool-and-deadlines).

### Source computation

See [source computation](/architecture/query-execution/#source-computation).

### Hot buffer integration

See [hot buffer integration](/architecture/query-execution/#hot-buffer-integration).

## Scheduled reports

[Reports and telemetry](/architecture/reports-telemetry/#scheduled-reports) now owns the reporting-window mechanism.

### Window modes

See [window modes](/architecture/reports-telemetry/#window-modes).

### Planned boundaries

See [planned boundaries](/architecture/reports-telemetry/#planned-boundaries).

### The watermark

See [the watermark](/architecture/reports-telemetry/#the-watermark).

### Catch-up and the clamp

See [catch-up and the clamp](/architecture/reports-telemetry/#catch-up-and-the-clamp).

### A worked example

See [the worked report example](/architecture/reports-telemetry/#a-worked-example).

### Editing a schedule

See [editing a schedule](/architecture/reports-telemetry/#editing-a-schedule).

## SSE streaming

See [SSE streaming](/architecture/query-execution/#sse-streaming) for pin snapshots and loss boundaries.

## Internal telemetry

See [internal telemetry](/architecture/reports-telemetry/#internal-telemetry) for persistence, bounded loss, and monitoring limits.
