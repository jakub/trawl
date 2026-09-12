---
title: Catalog and conformance
description: Understand why every field has one stored type and how conflicts are recorded.
---

Why does a field keep one type across the whole install? A query has to mean the same thing today as it did last week. If `duration` were an integer in one Parquet file and a string in the next, `duration > 100` would compare numbers in one file and text in the other. The field catalog pins one type per field name, and every writer and reader applies that pin.

A new field gets its pin at its first typed batch, and an all-null batch defers the decision. Trawl `TRY_CAST`s the non-null values against a fixed ladder, `BIGINT`, `DOUBLE`, `TIMESTAMP`, then `BOOLEAN`. The first candidate with at least a 90 percent success rate wins, and otherwise the field pins as `VARCHAR`. `SEVERITY` is declared or chosen by an operator, never inferred.

## Write-time conformance

Pins land in PostgreSQL and reach the in-process cache before any Parquet carrying them is published. If that write fails, compaction keeps the WAL and publishes nothing.

Conformance reads a column's text form, then applies a guarded cast. A bare `TRY_CAST` is not enough. Casting `1.5` to `BIGINT` rounds, and casting `TRUE` to boolean accepts a spelling the stored value cannot reproduce. A value outside the guarded domain becomes NULL in the typed column and counts as a conflict. The original text stays in `_raw`, subject to the ingest cap.

`TIMESTAMP` reads through `TIMESTAMPTZ`, applies an explicit offset, and treats a zoneless value as UTC. Every connection that conforms sets `TimeZone='UTC'`. The ladder scores with the same expressions that write, so the score and the stored value agree.

Compaction and rollup combine conformant files with `UNION ALL BY NAME`, and never resolve a type conflict by stringifying the column. An invariant violation keeps the inputs and reports the failure, because file damage and a restore against the wrong catalog need a person.

## Hot and cold agreement

A hot snapshot carries the pins for the keys it contains, recomputed on every snapshot call. A pin published a moment ago must not wait for the next ingest generation.

The query emitter conforms only the hot branch, because cold columns already carry the catalog's type on disk. Both branches use the same expressions, so the first Parquet file never changes how recent events read. Comparison typing uses the full catalog snapshot, not only the keys in the hot file. See [query execution](/architecture/query-execution/).

## Capacity

The catalog holds at most 10,000 pins. At capacity a new column is not stored in Parquet, and its values stay in `_raw`. One compaction batch may claim at most half the free slots. Boot conformance is exempt, since its columns are already on disk.

Alert on `trawl_catalog_pinned_fields` against `trawl_catalog_pin_capacity`, because the rejection counter reports exhaustion only after it happens. Ingest never reclaims a pin. Use `trawl schema gc-pins` to retire entries no current Parquet footer mentions.

Bookkeeping runs after Parquet publication, retrying transient failures inside a two-second budget per batch. Exhausting it records `catalog_bookkeeping_timeout` and increments `trawl_catalog_bookkeeping_timeouts_total`. A lost observation makes schema listings stale, but never revokes a durable Parquet write.

## Conflict evidence and degraded pins

`field_conflicts` keeps the newest 100 conflict rows per field, each with up to five sanitized samples of the rejected values. Durable per-field aggregates keep the episode count, the first and last conflict, and lifetime nulled-row totals after that window rolls off.

The analyzer turns that evidence into a verdict. A field degrades after conflicts span at least 24 hours and reach either 100 nulled rows or three episodes. The verdict is advisory and never triggers a repin on its own.

A query response can include `degraded_fields` for the fields the query bound, even when it projected them away. An SSE stream carries no equivalent notice. Fixing the sender does not recover the shelved values. Acknowledgement records an episode high-water mark and hides the badge until a new episode passes it.

## Repin

Repin retypes one field across the corpus through a shadow rewrite, and can recover values from `_raw`. Read [repin cutover](/architecture/recovery/#repin-cutover) for the mechanism and the [CLI reference](/reference/cli/) for the procedure.
