---
title: Catalog and conformance
description: How field types govern stored values, queries, and conflict evidence.
---

The field catalog is the install-wide authority for stored column types. A newly observed field gets a pin at its first typed batch; an all-null batch defers the decision. The inference ladder is `BIGINT`, `DOUBLE`, `TIMESTAMP`, `BOOLEAN`, then `VARCHAR`. `SEVERITY` is a declared or operator-selected type, not an inferred rung.

## Write-time conformance

Pins persist in PostgreSQL and reach the in-process cache before Parquet carrying those pins is published. If that write fails, compaction keeps the WAL and does not publish the batch. All normal Trawl compaction outputs conform to the catalog; a foreign file is outside that guarantee.

Conformance reads a column's text form before applying a guarded cast. Casting the type inferred for one JSON batch directly would give different answers depending on the other values in that batch. The shared expression builder is `trawl-core/src/conform.rs`; compaction, boot conformance, repin, and the query's hot branch use it.

A successful cast must satisfy the type's guard. Bare `TRY_CAST` is insufficient: casting `1.5` to `BIGINT` rounds, and casting `TRUE` to boolean accepts a spelling the stored value does not reproduce. A value outside the guarded domain becomes NULL in the column and counts as a conflict. The original remains available through `_raw`, subject to its ingest cap. DOUBLE accepts its native floating-point precision; it is not an exact decimal storage type.

TIMESTAMP reads through `TIMESTAMPTZ`, applies an explicit offset, and interprets a zoneless value as UTC. Connections that conform or read a conformed hot branch set `TimeZone='UTC'`. The type ladder uses the same expressions, so its scoring and the resulting writes agree.

Compaction and rollup combine conformant files with `UNION ALL BY NAME`. They do not retry a type conflict by converting all values to strings. An invariant violation retains the inputs and reports the failure. File damage, foreign data, and a restore against the wrong catalog require diagnosis rather than silent reconciliation.

## Hot and cold agreement

The hot snapshot carries a file and the pins for keys actually present in that snapshot. Its intersection is recomputed on every snapshot call, even when the file is cached: a newly published pin must not wait for a new ingest generation.

The query emitter conforms only the hot branch. Cold columns already carry the catalog's type. Hot-only queries use the same conformance expressions, so the first Parquet file does not change the interpretation of recent events. Comparison typing uses the full catalog snapshot, not just the keys present in the hot file. See [query execution](/architecture/query-execution/).

## Capacity and observations

The catalog admits at most 10,000 pins. At capacity, new unpinned columns are not stored in Parquet; their values remain in `_raw`. Compaction limits one batch to half the free slots. Boot conformance is exempt from that ration because its proposals describe columns already on disk.

Alert on `trawl_catalog_pinned_fields` relative to `trawl_catalog_pin_capacity`. The rejection counter reports exhaustion after it happens. Ingest does not reclaim pins. An operator can use the separate `schema gc-pins` proof over observation age and current Parquet footers to reclaim dead entries.

`field_services` records historical observation of each field/service pair. Retention does not remove these rows. Its field axis is bounded by pin capacity, but its service axis is not globally bounded. Readers therefore window on `last_seen` and page large service lists. The boot pass backfills observations using each file's partition hour, not boot time, and records completion separately from conformance.

Conflict and observation bookkeeping happens after Parquet publication. Live writes retry transient failures under a two-second wall-clock budget per batch. Exhausting that budget records `catalog_bookkeeping_timeout` and `trawl_catalog_bookkeeping_timeouts_total` for the write in flight. A failed observation can make schema freshness stale; it does not revoke a durable Parquet write.

## Conflict evidence and degraded pins

`field_conflicts` retains the newest 100 conflict rows per field. Each row can carry a bounded, sanitized sample of values rejected by the cast. Durable field/service aggregates retain episode count, first and last conflict, and lifetime nulled-row totals beyond that detail window.

The analyzer computes a verdict from that evidence. Degradation requires a span of at least 24 hours and either 100 nulled rows or three episodes. Multiple senders are displayed evidence, not a prerequisite. The verdict is advisory, and a sender can influence it by supplying conflicting data. It never authorizes automatic repin.

The schema refresh task caches the degraded set. A query response can include `degraded_fields` for fields the query bound, even if it projected them away. Freshness depends on the refresh tick; SSE carries no equivalent notice. Evidence has no recency expiry. Fixing the sender does not recover already shelved values or retire the badge by itself.

Acknowledgement records an episode high-water mark. It suppresses the badge only until a new episode exceeds that mark; it does not change stored values. Successful repin clears the old evidence and acknowledgement with the pin flip. A same-type forced repin can recover values from `_raw` without changing the type.

## Schema reads

The schema routes read catalog authority rather than infer types from arbitrary current files. Column listing joins pins with windowed observations. The retention horizon is the longest enabled environment age; an unlimited age removes that window. `all=true` lifts it, and a never-observed pin remains visible.

Unscoped column and corpus facts can be TTL-cached. Service-scoped listing is fresh rather than an unbounded cache indexed by sender-chosen names. A catalog-store outage returns an error, not an unannounced fallback to stale cache. Per-service file statistics remain physical facts; they do not override pinned types.

## Repin

Repin retypes one field across the corpus through a shadow rewrite, with possible recovery from `_raw`. The comparison contract defines its query effects; it does not promise identical results for every query. Read [recovery and cutover](/architecture/recovery/#repin-cutover) for the mechanism and the [CLI schema guide](/reference/cli/) for the procedure.

## Source owners

Implementation lives in `trawl-server/src/catalog/`, `src/store/catalog.rs`, `src/ingest/compaction.rs`, and `trawl-core/src/conform.rs`.
