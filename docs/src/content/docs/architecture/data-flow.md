---
title: Ingest and publication
description: Understand how an arriving event becomes durable, queryable, and then compacted Parquet.
---

Why is an event searchable a second after it arrives, yet still safe if the daemon stops? Durability and visibility are separate steps in a fixed order. `trawld` writes the event to the write-ahead log first, inserts it into the in-memory hot buffer next, and publishes it to live subscribers last. The WAL write is the durability record. The hot buffer is the visibility. Compaction later moves the event into Parquet, and no query counts it twice. A restart reloads the WAL that compaction has not merged, so an acknowledged event stays searchable across it.

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

A batch writes one file per environment and service group under `wal/{env}/`. The filename carries the validated service, the arrival milliseconds, and a random suffix. The writer creates a new `.tmp` file, writes and fsyncs it, hard-links it under its `.ndjson` name, removes the `.tmp`, then fsyncs the parent directory. The first write into an environment also fsyncs the WAL root, which holds the environment directory's entry. A write is acknowledged only after every fsync succeeds, so a successful write is durable before its events reach the hot buffer. Service names are validated, not rewritten, so two legal names never share a path.

The writer never replaces an existing file. The link fails when the name already exists, where a rename would replace the file. A name that exists, or that a pending [publication marker](/architecture/recovery/#publication-markers) in the environment lists, gets a fresh name. If eight names in a row are taken, the write fails, and the writer removes the `.tmp` and publishes nothing.

The writer reads the service's marker before it takes a name. If that marker is invalid, is over its size limit, or cannot be read, the writer cannot tell which names it claims. The write then fails the same way, before any link, and every write for that environment and service fails until the marker is resolved. A missing marker claims no name.

A failure after the link either completes the write, withdraws the file durably, or reports that the file stayed:

- If the `.tmp` cannot be removed, the write still succeeds and logs `wal_tmp_unlink_failed`. No WAL scan reads a `.tmp` file.
- If the directory fsync fails, the write fails. The writer removes the `.ndjson` file and fsyncs the directory again.
- If that removal or its fsync fails, the writer reports the file as left visible. Compaction can merge the file, or a power loss can bring it back, so an HTTP sender that retries the batch duplicates its rows. Telemetry does not retry such a batch.

## Hot buffer and the publication guard

The hot buffer holds shared batches until compaction drains them. Nothing else removes a batch. Each batch has one identity, `{env}/{WAL file stem}`, which compaction uses to drain it. A batch that arrives with an identity already resident is a writer bug: the buffer keeps the resident batch, logs `hot_buffer_duplicate_batch` at ERROR, and counts `trawl_hot_buffer_duplicate_batches_total`.

Queries share one temporary NDJSON snapshot per hot generation, and that snapshot carries the catalog pins for its own keys. If the snapshot cannot be built, the query or export answers 503 `service_unavailable` with a typed I/O cause kind. It never answers from Parquet alone, which would leave out the newest events.

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

At half of either cap, or after any refusal, compaction starts a pressure pass at once instead of waiting for its interval. A pressure pass reads WAL files of any age and skips daily rollup. Publication still checks under the write guard that every consumed file exists, so a withdrawn write is never published. A pass that failed stops pressure passes until the next regular pass, even when it drained other batches and the buffer is no longer under pressure. A stall makes its own inserts: its error lines reach internal telemetry, which lands in a healthy environment that the next pass could drain. A pass that found WAL files and drained none of them stops pressure passes the same way. A failure that stays until an operator fixes it, such as a blocking publication marker, a WAL file that always fails, or a WAL root entry that compaction cannot inspect, holds every service to the regular interval until the fault is fixed. A pass that drained something with no failure runs again at once while the buffer is under pressure or refusing. A pass that read WAL files of any age and found none ran before the admitted writes reached disk. It waits for their inserts, which wake compaction, and refusals alone do not start another pass. The regular interval runs throughout. The buffer reports that it is refusing until occupancy falls below a quarter of both caps.

When compaction cannot drain at all, for example while the catalog is down, ingest stays refused and reads stay complete. `/api/v1/health` reports `degraded` with `ingest_capacity` set to `refusing`, at HTTP 200. The [operational alerts](/operate/operational-alerts/#hot-buffer-drain-stalled) watch the oldest resident batch's age and sustained refusals.

## Restart visibility

What does a search see right after a restart? The hot buffer starts empty, but the WAL still holds every acknowledged event that compaction has not merged. Boot loads that WAL into the hot buffer before the server answers HTTP. This load is **hydration**. From the first response, a search counts each acknowledged event exactly once, or it answers 503 `corpus_recovering`. [Boot order](/architecture/recovery/#boot-order) lists the steps around hydration. The guarantee covers ingest nodes only. A query-only node never reads the WAL. [ADR-0041](https://github.com/jakub/trawl/blob/main/docs/adr/0041-an-acknowledged-event-is-counted-exactly-once-across-compaction-and-restart.md) records the design.

### Hydration

Hydration takes the WAL files that compaction would take, in every environment directory. That includes environments no longer in `[ingest] envs`, because the allowlist governs new ingest, not stored data. Hydration skips each environment and service that a pending publication marker blocks, and each scope it cannot list. It opens regular files only and never follows a symlink.

A file is hydrated only if its bytes are exactly what the WAL writer produces. Its name must be a writer name, `{service}_{unix_millis}_{4 hex digits}.ndjson`. It must be non-empty UTF-8 with no NUL byte and end in a newline. Each line must be a JSON object of scalar values with lowercase keys and no duplicate key, and serializing that object must give back the same bytes. Hydration loads whole files only and never canonicalizes an event again. A file that fails any check stays for compaction, whose decoder handles it as it always has. A second, looser decoder at boot could show different values hot than cold.

Hydration tries the files oldest first, by the time in each file name, and loads each file that fits. A file that does not fit is skipped, and later files are still tried, so the same WAL gives the same selection on every boot. It examines at most `hot_buffer_max_events` + 1 files, and reads at most `hot_buffer_max_bytes`, counting the bytes of files it then rejects. It counts a file's lines before it parses any of them, and skips a file with more lines than the event capacity left, so it never builds more events than fit.

Hydration charges the full caps, not the 15/16 share of HTTP and syslog. A backlog in the telemetry reserve would otherwise be left out, and search refused. Hydration is not a producer. It never refuses, never counts on `trawl_hot_buffer_admission_refusals_total`, and never wakes a pressure pass. A hydrated backlog at or above half of either cap leaves admission at pressure, and the first write that does not fit then moves it to refusing. Hydrated events do not go to the event bus, so live tail does not replay them, and they do not count as ingested traffic.

A hydrated batch keeps the identity that compaction drains it by, and takes its age from the time in its file name. After a restart, `trawl_hot_buffer_oldest_batch_age_seconds` therefore reports the age of the oldest surviving file, not the time since boot.

`boot_hydration` logs the counts per outcome, the events and bytes loaded, the bytes read, whether the examine bound was hit, whether overhang remains, and the duration. It names no path. `trawl_hydration_files_total{outcome}` counts the same outcomes. The [metrics reference](/reference/api/#restart-visibility-metrics) defines each outcome.

A file whose write was not acknowledged can come back. A file that the writer reported as left visible, or a removed file whose directory fsync was lost, reappears at boot and is searchable at once. If the sender retried that batch, its events are counted twice. [ADR-0043](https://github.com/jakub/trawl/blob/main/docs/adr/0043-ingest-is-admitted-against-hot-buffer-capacity.md) accepts the same trade for compaction.

### Overhang

WAL that existed at boot and did not become resident is the **overhang**. A file can be overhang because it did not fit, could not be read or recognized, sat in a blocked or unlisted scope, or lay past the examine bound. Overhang is a state of the publication gate, not a list of files. Nothing creates it after boot, because every later write reaches the hot buffer before its producer releases the ingest guard.

While overhang holds, a search cannot count every acknowledged event, so corpus reads answer 503 `corpus_recovering` with cause kind `restart_backlog`. A backlog larger than the caps, including one that fitted before the caps were lowered across a restart, causes a one-time overhang that compaction clears. A contradictory publication marker, a WAL file that always fails, or a catalog outage with a backlog above the caps keeps search refused until an operator fixes the fault.

Compaction clears overhang. Its first pass runs as soon as the compaction task starts. That pass takes WAL of any age and starts no daily rollup, and the rollup keeps its normal deadline. While overhang holds, every pass takes WAL of any age, as under admission pressure. A pass that drained something runs again at once, and a pass that drained nothing cools down until the next regular pass. A file that publication-marker recovery retires counts as drained.

After each pass's WAL phase, a coverage proof checks the WAL. It lists the WAL without the publication guard, then takes the write guard and checks again each listed file that is not resident. It clears overhang only if the listing was complete, no publication marker is pending, and every listed file is resident or gone. More than 1024 files that are not resident fail the proof, and a later pass tries again. `coverage_proof` logs each outcome, and `corpus_settled` logs once, when overhang clears.

### Reads while the corpus is unsettled

The corpus is unsettled while overhang holds or a [rollup marker](/architecture/recovery/#daily-rollup) is pending. Either way, a read cannot count every acknowledged event exactly once. These requests answer 503 `corpus_recovering`:

- queries and exports
- field values that are not cached
- a manual run of a saved query, which creates no run

The cause kind is `rollup_pending` while a rollup marker is pending, and `restart_backlog` otherwise. The response carries no `Retry-After`: the next pass may clear the state at once, and a standing fault never clears without an operator.

A repin is refused before its scan, and the job ends `blocked` with a message that names the reason. The scheduler skips its whole poll while the corpus is unsettled, so it claims no window and no fire cursor moves. After the corpus settles, one claim covers the gap.

These answer as usual: live tail over SSE, a `| from saved` query, which reads saved report files only, `/api/v1/schema`, `/api/v1/schema/services`, `/api/v1/health`, `/metrics`, and a cached field-values hit. The schema routes count from Parquet footers, so their counts are cold-only and approximate in any case.

`/api/v1/health` names the state in `checks.corpus` and reports `degraded` at HTTP 200, so probes do not restart the server. `trawl_corpus_unsettled{reason}` reads 1 for each reason that holds. [Corpus unsettled](/operate/operational-alerts/#corpus-unsettled) is the runbook for the alert that fires after ten minutes.

## Compaction

The compactor finds eligible WAL files, groups them by service and environment, takes catalog pins, conforms the batch, and merges it into the existing hourly file. A [publication marker](/architecture/recovery/#publication-markers) records each publish before the output is renamed into place. Publication and hot drain happen together under the write guard. The consumed WAL files are then retired, the marker is removed, and bookkeeping runs. Failed work keeps its inputs for diagnosis. Read [catalog conformance](/architecture/catalog/#write-time-conformance) for the cast rules.

Compaction reads each row's `_time` and `_ingested` with `TRY_CAST`. If a value is unusable, it falls back to the arrival instant in that row's own WAL filename, then to the compaction instant. The fallback is per row. Canonicalized events already have valid timestamps.

Hourly files then consolidate into one daily file per service, ordered by `_time`. See [rollup recovery](/architecture/recovery/#daily-rollup).

While [overhang](#overhang) holds, compaction runs a coverage proof after each pass's WAL phase, before the rollup.

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
