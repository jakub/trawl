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

A batch writes one file per environment and service group under `wal/{env}/`. The filename carries the validated service, the arrival milliseconds, and a random suffix. The writer creates a `.tmp` file, fsyncs its data, renames it to `.ndjson`, then fsyncs the parent directory. The first write into an environment also fsyncs the WAL root, which holds the environment directory's entry. A write is acknowledged only after every fsync succeeds, so a successful write is durable before its events reach the hot buffer. If a directory fsync fails, the write fails and the writer tries to remove the renamed file. If the removal fails too, the file stays in the WAL and compaction merges it, so an HTTP sender that retries the batch duplicates its rows. Telemetry does not retry such a batch. Service names are validated, not rewritten, so two legal names never share a path.

## Hot buffer and the publication guard

The hot buffer holds shared batches until compaction drains them. Nothing else removes a batch. Queries share one temporary NDJSON snapshot per hot generation, and that snapshot carries the catalog pins for its own keys.

A publication lock keeps a query from seeing one event twice. A query takes the read guard before it selects files and holds it through its last physical read. Compaction takes the write guard for Parquet publication and hot drain together, so no query sees the moment between them. Producers hold the ingest read guard from the WAL write through hot insertion, so a slow producer cannot reinsert a drained batch.

Daily rollup holds the same exclusion through hourly-file retirement. These guarantees cover one daemon. They do not deduplicate client retries or coordinate independent readers of the same files.

A bounded Tokio broadcast channel shares each batch with live subscribers. A slow consumer lags and gets a loss notification rather than blocking the producer. The bus is not a durable replay queue.

### Hot-buffer admission

What happens when events arrive faster than compaction drains them? The hot buffer refuses new writes and keeps every event it already holds. A refused sender can retry. An acknowledged event that no query could see would be silent loss, so the buffer never removes one to make room.

The buffer keeps one ledger of events and serialized ndjson bytes, capped by `[ingest] hot_buffer_max_events` and `hot_buffer_max_bytes`. A producer reserves space for its whole batch before it writes the WAL. The order is parse, reserve, take the ingest read guard, write the WAL, and insert. A refused batch writes nothing and never touches the publication lock. Compaction needs that lock's write side to drain, and it is the only thing that frees space, so nothing waits for capacity while it holds the guard. A reservation that is not used, because a WAL write failed or a request was cancelled, returns its space at once.

HTTP and syslog may fill 15/16 of each cap. Internal telemetry may fill the whole cap, so trawld's own records of a stall stay searchable during it. The producer is known from the code path that wrote the batch, never from an event field, so a client that sends `service=trawld` gets the external share.

Each producer handles a refusal its own way:

- **HTTP** answers 503 `hot_buffer_full` with `Retry-After` set to the compaction interval when the request fits the external share but not the free space. It answers 413 `ingest_batch_too_large` when the request is larger than the external share and can never fit. When the buffer has no free space at all, the request is refused before its body is decompressed. See [ingest events](/reference/api/#ingest-events).
- **Syslog** keeps a refused group pending, in arrival order, and stops taking frames from its queue until space frees. TCP listeners then stop reading, so the kernel's flow control slows the sender, and the connection stays open. UDP has no flow control: a datagram that finds the queue full is dropped and counted. See [syslog delivery under load](/operate/ingestion/#syslog-delivery-under-load).
- **Internal telemetry** keeps a refused batch at the front of its queue and tries again on the next flush. See [internal telemetry](/architecture/reports-telemetry/#internal-telemetry).

At half of either cap, or after any refusal, compaction starts a pressure pass at once instead of waiting for its interval. A pressure pass reads WAL files of any age and skips daily rollup. Publication still checks under the write guard that every consumed file exists, so a withdrawn write is never published. A pass that failed stops pressure passes until the next regular pass, even when it drained other batches and the buffer is no longer under pressure. A stall makes its own inserts: its error lines reach internal telemetry, which lands in a healthy environment that the next pass could drain. A pass that found WAL files and drained none of them stops pressure passes the same way. A pass that drained something with no failure runs again at once while the buffer is under pressure or refusing. A pass that read WAL files of any age and found none ran before the admitted writes reached disk. It waits for their inserts, which wake compaction, and refusals alone do not start another pass. The regular interval runs throughout. The buffer reports that it is refusing until occupancy falls below a quarter of both caps.

When compaction cannot drain at all, for example while the catalog is down, ingest stays refused and reads stay complete. `/api/v1/health` reports `degraded` with `ingest_capacity` set to `refusing`, at HTTP 200. The [operational alerts](/operate/operational-alerts/#hot-buffer-drain-stalled) watch the oldest resident batch's age and sustained refusals.

## Compaction

The compactor finds eligible WAL files, groups them by service and environment, takes catalog pins, conforms the batch, and merges it into the existing hourly file. A [publication marker](/architecture/recovery/#publication-markers) records each publish before the output is renamed into place. Publication and hot drain happen together under the write guard. The consumed WAL files are then retired, the marker is removed, and bookkeeping runs. Failed work keeps its inputs for diagnosis. Read [catalog conformance](/architecture/catalog/#write-time-conformance) for the cast rules.

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
