---
title: Recovery and cutover
description: Understand the markers Trawl writes on disk and how an interrupted operation finishes.
---

Why can Trawl finish an interrupted rewrite instead of guessing what happened? Every long operation that touches files writes a marker beside the data it governs, before it starts. A marker is durable state, not a lockfile. On the next boot Trawl reads the markers. It finishes or rolls back each operation it can prove, and it blocks the work it cannot prove safe. Nothing rewrites or deletes a file it cannot prove it wrote.

## Boot order

Boot finishes the interrupted work it can before the server answers HTTP. An ingest node runs these steps in order:

1. Check the storage format marker, `data/EPOCH`.
2. Run repin filesystem recovery.
3. Recover publication markers.
4. Reconcile repin state with PostgreSQL, then build the server state. The publication gate scans for rollup markers here.
5. Recover rollup markers.
6. Run boot conformance.
7. Hydrate the surviving WAL into the hot buffer.
8. Activate self-telemetry, start compaction, the other workers, and the scheduler, then bind the HTTPS listener.

Every response the server sends therefore comes after recovery and hydration. Hydration must finish before self-telemetry starts: a WAL file that telemetry wrote earlier would reach the hot buffer twice, once when it was inserted and once from hydration. [Restart visibility](/architecture/data-flow/#restart-visibility) explains hydration and the overhang it can leave.

A query-only node checks the format marker and the repin state, builds its state, and checks that the archive belongs to the connected catalog. It skips steps 3, 5, 6, and 7, and it never reads the WAL. If it finds `*.ndjson` files under the WAL root, it logs one `wal_present_on_query_node` WARN with the file count and no path. Their rows are missing from its answers until an ingest node compacts them. The restart guarantee covers ingest nodes only.

## Storage format marker

`data/EPOCH` identifies the storage format. Startup creates a missing data root and writes the current value, `3`, even with ingestion disabled. An unknown marker stops startup instead of letting the daemon guess. Keep this file with the corpus when you back up or restore data.

A query-only daemon leaves an existing root without this marker untouched. The catalog identity check still applies.

## Boot conformance and the CATALOG marker

`data/CATALOG` binds the corpus to the catalog that typed it. An ingest boot without matching proof scans the owned Parquet files, seeds missing pins, rewrites nonconformant values, fills the pin cache, and publishes the marker last. A restart keeps every completed rewrite.

Only Trawl-shaped paths qualify: a valid environment, service, date, and hour. `scheduled/` is excluded. An unreadable or foreign file stays byte-identical and holds back marker publication, logging `catalog_conform_skip` and `catalog_conform_incomplete`. One such file does not stop the daemon serving, but a query touching it can fail. A failed conformance or a failed required backfill is fatal to an ingest boot.

Seeding votes count only the rows that carry the unpinned field. Observation backfill uses partition time and tracks its own completion. Progress logs separate a long scan from a stalled startup.

A query-only node runs no rewrite. A marker naming a different catalog is fatal. An absent marker is unproven rather than proof of another identity, so the node warns and serves. Restoring only Parquet, or only PostgreSQL, changes the identity verdict, and neither is a complete restore.

## Repin cutover

One persisted job at a time can retype a field. Every request, including a dry run, claims the job and scans the corpus with the same expressions that rewrite values. A dry run skips the corpus replacement and still records job state.

The shadow root is `data.repin-next/`, a sibling of the live data root. Unaffected files are hardlinked, and affected owned files are rewritten. The new value comes from the guarded stored reading first, then from `_raw`, which can bring back shelved values.

Staging inside the data root would let a recursive query glob see the same data twice, so the shadow has to be a sibling. That requires one filesystem, so a data root that is itself a mount point, a nested corpus mount, or a symlink under an environment is refused before the build starts. A staging root left behind by a failed cleanup refuses the next build rather than being overwritten.

Ingest continues during the build, daily rollup pauses for the job, and a bounded catch-up folds in the compaction that happened meanwhile. Force ceilings are checked against the scan and again against the finished shadow. An unspecified ceiling is the scan count plus ten rows or ten percent, whichever is larger. A 202 response is not a promise of cutover, because the finished shadow can still exceed the accepted loss and be refused.

Cutover takes the compaction corpus gate and exclusive query permits, applies the final increment, writes the `data/REPIN` marker, swaps each environment with two renames, then flips the PostgreSQL pin and the cache. Exclusion matters because a mix of scalar files can silently promote types rather than fail. The same transaction stores the rewrite's conflict evidence, and the job reports `succeeded` only once that evidence is durable.

Cancellation is cooperative before cutover and cannot undo the committed phase. Past the cutover marker, recovery goes forward. If your request disconnects or times out, read the job status before you retry.

## Repin recovery and retention

Boot runs filesystem recovery after the storage-format check, then reconciles PostgreSQL before any corpus reader starts. A building marker abandons the shadow, a cutover marker completes the swaps, and cleanup retries its own sweep. A query-only start serves past a building marker but refuses an unfinished cutover.

Retention stands down while repin state makes deletion unsafe. It rechecks immediately before deleting, so a job admitted during a sweep stops the rest of it. A failed staging cleanup keeps the marker that licenses another attempt. `trawl_retention_suppressed` shows this even when no job reports running.

A query-only node can hold stale pins until you restart it. Repin is a single-daemon exclusion contract, not coordination for independent readers.

## Daily rollup

Rollup writes a durable marker listing its complete input set before replacing hourly files with the daily output, and holds the publication write guard through input retirement. An unfinished marker refuses corpus reads with 503 `corpus_recovering` and cause kind `rollup_pending` until recovery completes, including across a restart. That is what stops a query counting the hourly and daily copies of one event.

An ingest boot recovers rollup markers before boot conformance, so a marker that recovery can finish never reaches a client. Recovery renames or retires files and reads Parquet footers. It never merges again. `rollup_boot_recovery` logs the marker count at INFO. A failure does not stop the boot. `rollup_boot_recovery` then logs at ERROR with the count still pending, reads stay refused as `rollup_pending`, and each compaction pass retries. Boot conformance leaves a day directory alone while a rollup marker in it is unresolved, and logs `catalog_conform_skip` with `phase="rollup"` for each file it skips. If conformance cannot read the rollup markers at all, it skips every file and logs `catalog_conform_rollups_unreadable`.

If the gate's scan for rollup markers fails, the gate cannot know which markers exist, so reads refuse as `rollup_pending`. Only a later complete scan clears that. On an ingest node, each compaction pass scans again, and `publication_scan_recovered` logs the scan that clears it. A query-only node has no compaction, so its failed scan stays until you restart it.

## Publication markers

Why can a crash in the middle of compaction not count an event twice? Compaction moves rows from WAL files into one hourly Parquet file. If the rows reach Parquet and the WAL files survive, the next compaction merges them again. A publication marker records each publish, so recovery can tell which side of the rename a crash landed on.

The marker lives at `wal_dir/{env}/.publish-{service}.json`, one for each environment and service. It names the consumed WAL files, the temporary output, the canonical Parquet file, and the canonical file's identity: its size and its BLAKE3 digest. The `.json` suffix keeps the marker out of the WAL scan, which reads only `*.ndjson` files.

### Publish order

Compaction publishes each output in five steps:

1. Fsync the temporary output and every directory leading to it, then hash it.
2. Write the marker durably.
3. Take the publication write guard. Rename the output into place, drain the matching batches from the hot buffer, and fsync the output directory.
4. Retire the consumed WAL files, then fsync the WAL directory. Retirement deletes each file, or renames it out of the WAL scan if the delete fails.
5. Remove the marker durably.

A failure before the rename leaves the WAL in place, and recovery rolls the publish back. A failure after the rename leaves the marker, and recovery completes the publish. Catalog bookkeeping runs after step 5 and does not affect the outcome.

### How recovery decides

Recovery reads the canonical file and compares it with the identity that the marker records. Size and digest are the only evidence. A missing temporary output never proves that the publish happened, because an earlier recovery can have deleted that file before it crashed.

Each marker gets one outcome, counted on `trawl_publication_recovery_total{outcome}`:

- `published`: the canonical file carries the recorded identity. Recovery fsyncs the output directory, drains the hot batches, retires the WAL files, and removes the marker.
- `unpublished`: the canonical file lacks the identity and the temporary output exists. Recovery deletes the temporary output and the marker. The WAL files stay for the next compaction.
- `contradictory`: the evidence does not fit either case. For example, both outputs are missing, or the canonical file has a different identity and the temporary output is gone. An invalid marker, or a named path that is not a regular file, is also contradictory. Recovery changes nothing.
- `failed`: a filesystem error stopped recovery of one marker, or recovery could not list the markers. The next pass retries while the marker remains. Recovery removes a marker only after its outcome is settled, so an error after that leaves nothing to retry: the rows are already published, or still in the WAL for the next compaction. A temporary output left behind is removed by stale temporary-file cleanup.

Every recovery step can be interrupted and run again, and both branches reach the same end state. A contradictory or failed marker fires `TrawlPublicationRecoveryBlocked`. Read the [publication recovery runbook](/operate/operational-alerts/#publication-recovery-blocked) before you touch the marker. Removing it makes compaction merge the listed WAL files again, which duplicates their rows if they were already published.

### When recovery runs

An ingest-enabled boot recovers publication markers after the storage-format check and the repin filesystem recovery. That is before boot conformance can rewrite a canonical file, and before any reader, producer, or compaction tick exists. A query-only node does not run publication recovery. Only an unreadable WAL root stops the boot. A marker that recovery cannot resolve is logged and counted, and the daemon starts. Hydration then skips the scope that the marker blocks, so search answers 503 `corpus_recovering` with cause kind `restart_backlog` until an operator resolves the marker and compaction drains the scope.

Every compaction tick runs recovery again before it merges any WAL file or cleans up a temporary output. The tick holds the repin corpus read guard, and it drains hot batches under the publication write guard.

### What a pending marker blocks

A marker claims the files it names until recovery removes it. While it exists:

- compaction skips that environment and service.
- stale temporary-file cleanup keeps the temporary output.
- daily rollup skips that service's day.
- retention keeps that date directory.
- a repin cutover is refused.
- boot hydration skips that environment and service.
- the WAL writer does not reuse a file name that the marker lists. If the marker is invalid or cannot be read, the writer refuses every write for that environment and service instead.

### Directory fsync is part of the acknowledgement

The marker protocol assumes that an existing WAL file holds rows that are not yet in Parquet. That holds only if every acknowledged WAL file is durable. The WAL writer therefore acknowledges a write only after the WAL directory fsync succeeds. The first write into an environment also fsyncs the WAL root. If a directory fsync fails, the write fails, and the writer removes the new `.ndjson` file and fsyncs the directory again. If the removal or that second fsync fails, the writer reports the file as left visible. Compaction can then merge it, and a power loss can bring back a removal that was not synced, so an HTTP sender that retries the batch duplicates its rows. Each producer handles the failure on its existing path:

- HTTP ingest answers a redacted HTTP 500.
- Syslog discards the group and counts it.
- Self-telemetry keeps the batch for retry. If the writer reported the file as left visible, telemetry releases the batch instead, because compaction can still merge that file.

The writer also never replaces an existing WAL file. It publishes with a hard link, which fails when the name exists, so a name that a file or a pending marker already holds gets a fresh one.

[Ingest and publication](/architecture/data-flow/#wal-writer) describes the WAL writer.

### Shutdown

Compaction stops at shutdown without a final pass. WAL files that it has not merged stay for the next run. The next boot recovers any publish that the stop interrupted, then hydrates the WAL that remains.

### Limits

The protocol does not cover every failure:

- A successful fsync is the only durability proof that Trawl uses. After a failed fsync, some Linux filesystems drop the unwritten pages and report a later fsync as successful. Recovery fsyncs again and cannot detect that case.
- Retention re-reads the publication markers immediately before it deletes each date directory. Retention never deletes the date that it reads as today. A publish that starts before midnight can still write into yesterday's directory while retention, already on the new date, deletes that directory. If compaction writes its marker between the re-read and the delete, each of that publish's rows is either still in the WAL or was published into the deleted date. No acknowledged row is counted twice, and no row is lost that retention was not already deleting with its date. The marker can outlive its output. Recovery then reports it as `contradictory`, `TrawlPublicationRecoveryBlocked` fires, and the service stays blocked until an operator resolves the marker.

## Recovery is not backup

Markers make an interrupted owned operation restartable. They do not replace a backup of the corpus, the Trawl database, the Fleet keystore, and the session material. Restore those together and test the result. Removing a marker to make a failed startup look clean destroys the evidence that would finish the job.
