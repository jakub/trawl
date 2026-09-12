---
title: Ingest and publication
description: Understand how an arriving event becomes durable, queryable, and then compacted Parquet.
---

Why is an event searchable a second after it arrives, yet still safe if the daemon stops? Durability and visibility are separate steps in a fixed order. `trawld` writes the event to the write-ahead log first, inserts it into the in-memory hot buffer next, and publishes it to live subscribers last. The WAL write is the durability record. The hot buffer is the visibility. Compaction later moves the event into Parquet, and no query counts it twice.

```text
HTTP, syslog, or internal telemetry
          │
    canonical event
          │
     durable WAL write
          │
   hot buffer and event bus ──> live subscribers
          │
    hourly compaction
          │
      Parquet files ──> daily rollup
```

## Canonicalization

Every accepted event passes through one canonicalizer. HTTP senders use bearer-authenticated ingest. Syslog and internal telemetry supply producer profiles that assert the identity their transport establishes. The [event reference](/reference/events/) defines the declared fields, the repairs, and the [producer profiles](/reference/events/#producer-profiles).

The syslog listener publishes its frame values as ordinary `syslog_*` and `sd_*` fields, using the fixed [severity](/reference/events/#severity-derivation) and [time](/reference/events/#time-derivation) rules.

## WAL writer

A batch writes one file per environment and service group under `wal/{env}/`. The filename carries the validated service, the arrival milliseconds, and a random suffix. The writer creates a `.tmp` file, fsyncs its data, renames it to `.ndjson`, then fsyncs the parent directory. A successful write is durable before its events reach the hot buffer. Service names are validated, not rewritten, so two legal names never share a path.

## Hot buffer and the publication guard

The hot buffer holds shared batches until compaction drains them or capacity forces eviction. Queries share one temporary NDJSON snapshot per hot generation, and that snapshot carries the catalog pins for its own keys.

A publication lock keeps a query from seeing one event twice. A query takes the read guard before it selects files and holds it through its last physical read. Compaction takes the write guard for Parquet publication and hot drain together, so no query sees the moment between them. Producers hold the ingest read guard from the WAL write through hot insertion, so a slow producer cannot reinsert a drained batch.

Daily rollup holds the same exclusion through hourly-file retirement. These guarantees cover one daemon. They do not deduplicate client retries or coordinate independent readers of the same files.

A bounded Tokio broadcast channel shares each batch with live subscribers. A slow consumer lags and gets a loss notification rather than blocking the producer. The bus is not a durable replay queue.

## Compaction

The compactor finds eligible WAL files, groups them by service and environment, takes catalog pins, conforms the batch, and merges it into the existing hourly file. Publication and hot drain happen together under the write guard, then bookkeeping runs and the processed WAL files are removed. Failed work keeps its inputs for diagnosis. Read [catalog conformance](/architecture/catalog/#write-time-conformance) for the cast rules.

Compaction reads each row's `_time` and `_ingested` with `TRY_CAST`. If a value is unusable, it falls back to the arrival instant in that row's own WAL filename, then to the compaction instant. The fallback is per row. Canonicalized events already have valid timestamps.

Hourly files then consolidate into one daily file per service, ordered by `_time`. See [rollup recovery](/architecture/recovery/#daily-rollup).

## Retention

Retention removes date directories inside individual environments. The age policy is per environment, while disk-pressure selection ranks candidates across all environments by the fraction of their age allowance consumed. An unlimited age does not exempt data from disk pressure. Recovery state can suppress deletion. See [configuration](/reference/configuration/#retention).

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

The tree shows both event-file shapes. A completed rollup retires its hourly inputs, so queries must not count both copies. The WAL can use its own configured directory. Repin stages a sibling root, never a subdirectory here.

Recovery explains the [EPOCH](/architecture/recovery/#storage-format-marker) and [CATALOG](/architecture/recovery/#boot-conformance-and-the-catalog-marker) markers.

Read [query execution](/architecture/query-execution/) next, or [reports and telemetry](/architecture/reports-telemetry/) for scheduled windows.
