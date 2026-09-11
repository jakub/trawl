---
title: Recovery and cutover
description: Storage epochs, catalog identity, rollup recovery, and repin exclusion.
---

Trawl stores recovery evidence beside the data it governs. Markers describe durable state, not disposable lockfiles. Recovery must establish ownership before rewriting or deleting files.

## The epoch cutover

`data/EPOCH` currently contains `3`. An ingest-enabled daemon handles roots as follows:

| Root | Boot behavior |
| --- | --- |
| Current epoch 3 | Serve normally; warn if an old set-aside remains. |
| Epoch 2 | Move it to `data.pre-epoch-3/` and create a fresh current root. Refuse an ambiguous existing destination. |
| Marker-less legacy Trawl root | Move it to `data.pre-schema-v2/` and create a fresh current root. |
| Marker-less directory with no Trawl ownership evidence | Adopt it in place instead of renaming an arbitrary directory. |
| Unknown epoch marker | Refuse rather than guess its format. |

Fresh-root publication is staged and durable. A restart between the set-aside rename and fresh-root publication resumes the operation. Trawl does not delete the set-aside for the operator. An existing set-aside suppresses disk-pressure retention; age retention continues unless another gate suppresses it.

An external legacy WAL directory has its own ownership and cutover check. A directory containing old flat WAL files can be set aside even when an empty data root is adopted in place. Report results under `scheduled/` are carried back to the current root because their relative paths remain in PostgreSQL. If both locations contain report results, recovery reports a conflict rather than merging them blindly.

A query-only daemon does not own the archive. It leaves a marker-less root untouched, and warns while serving an epoch-2 root rather than moving it. Its catalog-identity checks still apply.

The exact boot table and filesystem ownership predicates live in [epoch.rs](https://github.com/jakub/trawl/blob/main/crates/trawl-server/src/epoch.rs).

## Boot conformance and the CATALOG marker

`data/CATALOG` binds the corpus to its catalog identity. An ingest-enabled boot without matching proof scans owned Parquet files, seeds missing pins, rewrites nonconformant values, fills the pin cache, and publishes the marker last. Restarts retain each durable completed rewrite.

Only Trawl-shaped paths with valid environment, service, date, and hour components are eligible. `scheduled/` is excluded. An unreadable or foreign file stays byte-identical and holds back marker publication. This produces `catalog_conform_skip` and `catalog_conform_incomplete`; one such file does not prevent serving, but a query touching it can fail. A conformance or required observation-backfill failure is fatal to the ingest boot.

Seeding votes are weighted by rows that actually carry the unpinned field, not the total rows in a file. Already-pinned columns need no vote. Observation backfill uses partition time and has its own completion flag. Progress logs distinguish a long scan from a stalled startup.

A query-only node runs no rewrite. A marker naming a different catalog is fatal; an absent marker is unproven rather than proof of a different identity, so the node warns and serves. Restoring only Parquet or only PostgreSQL can therefore change the identity verdict and must not be treated as a complete restore.

## Repin cutover

One persisted job at a time can retype a field. Every request, including dry run, claims the job and scans the corpus. The scan counts with the same expressions used to rewrite values. Dry run avoids corpus replacement but still records job state.

The shadow is `data.repin-next/`, beside the live data root. Unaffected files are hardlinked; affected owned files are rewritten. The target value uses the guarded stored reading first, then guarded recovery from `_raw`. This can resurrect values an earlier pin shelved, provided the raw representation retained them.

Staging inside the data root would make recursive query globs see duplicate data. Sibling staging requires one filesystem: a data root that is itself a mount point, nested corpus mounts, or symlinks under an environment cause refusal before building. Put the root inside its volume. A previous staging root whose cleanup failed is recovery evidence, not an invitation to overwrite it.

Ingest can continue during the build. Daily rollup pauses for the job, and bounded additive catch-up includes intervening compaction. Force ceilings are checked both against the scan and the finished shadow. If unspecified, each ceiling derives from the scan count plus the greater of ten rows or ten percent rounded up. A 202 response does not guarantee cutover: the finished shadow can still exceed accepted loss and end refused.

Cutover takes the compaction corpus gate and exclusive query permits, applies the final increment, writes the `REPIN` marker, swaps each environment with two renames, and flips the PostgreSQL pin and cache. The exclusion matters because mixed scalar files can silently promote types rather than fail. The terminal success boundary also includes the required conflict evidence described in [ADR-0022](/contribute/decisions/#adr-0022).

Cancellation is cooperative before cutover and cannot undo the committed phase. Past the cutover marker, recovery proceeds forward. A disconnected or timed-out caller must inspect the job rather than retry blindly.

## Repin recovery and retention

Boot handles filesystem recovery before the epoch gate, then reconciles PostgreSQL after app-state startup. A building marker abandons the shadow; a cutover marker completes swaps; cleanup retries its owned sweep. Query-only startup can serve a building marker but refuses an unfinished cutover.

Retention stands down while repin state makes deletion unsafe. It rechecks immediately before deletions so a job admitted during a sweep stops subsequent removal. Failed staging cleanup retains the marker that licenses another attempt. `trawl_retention_suppressed` detects this condition even when no job reports running.

Existing SSE streams keep their original pins until reconnect. Query-only nodes can retain stale pins until restart. Repin is a single-daemon exclusion contract, not distributed coordination for independent readers.

## Daily rollup

Rollup publishes a durable marker listing its complete input set before replacing hourly files with daily output. It holds the publication write guard through input retirement. An unfinished marker refuses corpus reads until recovery completes, including after restart. This prevents double-counting hourly and daily copies.

## Recovery is not backup

Markers make interrupted owned operations restartable. They do not replace a backup of the corpus, Trawl database, Fleet keystore, and relevant session material. A restore must preserve their relationships and be tested against the selected release. Do not remove markers to make a failed startup appear clean.

Decision owners: [ADR-0011](/contribute/decisions/#adr-0011), [ADR-0019](/contribute/decisions/#adr-0019), [ADR-0022](/contribute/decisions/#adr-0022), and [ADR-0026](/contribute/decisions/#adr-0026). Implementation owners are `src/repin/`, `src/epoch.rs`, `src/catalog/conform.rs`, and rollup/publication code in `trawl-server`.
