---
title: Recovery and cutover
description: Understand the markers Trawl writes on disk and how an interrupted operation finishes.
---

Why can Trawl finish an interrupted rewrite instead of guessing what happened? Every long operation that touches files writes a marker beside the data it governs, before it starts. A marker is durable state, not a lockfile. On the next boot Trawl reads the markers and either finishes the operation or refuses to serve. Nothing rewrites or deletes a file it cannot prove it wrote.

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

Boot handles filesystem recovery before the storage-format check, then reconciles PostgreSQL after app-state startup. A building marker abandons the shadow, a cutover marker completes the swaps, and cleanup retries its own sweep. A query-only start serves past a building marker but refuses an unfinished cutover.

Retention stands down while repin state makes deletion unsafe. It rechecks immediately before deleting, so a job admitted during a sweep stops the rest of it. A failed staging cleanup keeps the marker that licenses another attempt. `trawl_retention_suppressed` shows this even when no job reports running.

A query-only node can hold stale pins until you restart it. Repin is a single-daemon exclusion contract, not coordination for independent readers.

## Daily rollup

Rollup writes a durable marker listing its complete input set before replacing hourly files with the daily output, and holds the publication write guard through input retirement. An unfinished marker refuses corpus reads until recovery completes, including across a restart. That is what stops a query counting the hourly and daily copies of one event.

## Recovery is not backup

Markers make an interrupted owned operation restartable. They do not replace a backup of the corpus, the Trawl database, the Fleet keystore, and the session material. Restore those together and test the result. Removing a marker to make a failed startup look clean destroys the evidence that would finish the job.
