# Malformed ingest timestamps are substituted, never fatal

status: accepted (2026-07-28)

`timestamp` is trawl's partition and sort key: compaction writes
`data/{date}/{HH}/{service}.parquet` ordered by it, row-group min/max stats on
it are the primary pruning mechanism, and every `last=Xh` filter ranges over it.
That privileged status was expressed as a **hard** `CAST("timestamp" AS
TIMESTAMP)` in the emitted SQL — at `compaction.rs` when building a WAL batch,
and at `emitter/state.rs` when unioning the hot buffer with parquet.

A hard CAST makes one malformed value fatal to everything around it. Ingest
validates only `service`, and `fill_defaults` substitutes a timestamp only when
the field is *absent* — so a present-but-unparseable value (`"not-a-date"`, a
nested object, a bare epoch int) is accepted with HTTP 200 and reaches the WAL.
There it fails the batch CAST. The isolation path cannot contain it: `probe_ndjson`
is a pure structural probe with no CAST, so the file parses cleanly, is retained
as a survivor, and the batch fails again — retried every tick forever, WAL never
drained, never quarantined, never reclaimed by retention. Separately, the same
CAST on the hot/cold union throws, `is_union_type_conflict` misreads a data
error as a schema conflict, the coerced retry cannot help (it deliberately never
coerces the partition key), and the query silently degrades to hot-only —
dropping the entire parquet history for that service.

This also diverged from ADR-0001, which already established that the streaming
path coerces unparseable strings to `Null` rather than failing. Batch was the
outlier.

## Decisions

- **The partition key is never hard-CAST in emitted SQL.** Every
  `CAST("timestamp" AS TIMESTAMP)` becomes `TRY_CAST`, in both the compaction
  and hot/cold-union paths. One bad row degrades one row. This is the invariant;
  the rest of this ADR exists because `TRY_CAST` alone yields NULL, and a NULL
  partition key is its own silent failure — the row sorts first and falls outside
  every time filter, present on disk and invisible to essentially every query.
- **Malformed time is substituted and preserved, never dropped.** At ingest, an
  unparseable `timestamp` is replaced with the request-arrival value
  `fill_defaults` would have used, and the original is moved to
  `timestamp_invalid`. The event stays queryable; the evidence an operator needs
  to fix their upstream Vector remap survives with it. This extends the existing
  documented intent of `fill_defaults` — *"we fill sensible defaults at ingest
  time rather than silently dropping them"* — from the absent case to the
  malformed one, where it was always meant to apply.
- **We do not reject the event.** Rejecting would be more symmetrical with how a
  bad `service` is handled, but `service` is a routing key with no sane default
  (it names the file), whereas time has one: arrival. Dropping log lines because
  a producer emitted a bad clock value is the wrong trade for a log platform.
- **Compaction falls back to WAL-filename time.** WAL files are named
  `{service}_{unix_millis}_{4_hex_random}`, so the ingest instant is recoverable
  from the path. Where `TRY_CAST` yields NULL, compaction coalesces to that
  value. This keeps rows inside `last=Xh` ranges and — the reason it matters
  operationally — drains WAL already wedged on disk from before this change with
  no manual recovery step.
- **Error classification does not sniff strings.** `is_union_type_conflict`
  matched on `msg.contains("Conversion")`, which both misfires on data errors
  and misses the STRUCT/BIGINT cast failures entirely — the latter taking a path
  that drops cold data with no warning logged at all. A cold-data drop must never
  be silent, so classification keys off structured error information and the
  no-warning path is removed.

## Consequences

- `timestamp_invalid` is a sparse column; it is absent from the vast majority
  of files. `union_by_name=true` reconciles it across files, but only if
  `DuckDB` detected it at all: JSON schema detection samples a bounded prefix
  (~20480 rows) by default, so every `read_json` over WAL or a hot-buffer
  snapshot sets `sample_size=-1`. Without it a repaired event past the prefix
  is dropped from the inferred schema with no error — the preservation this
  ADR promises would silently not happen on exactly the high-volume services
  where it matters most.
- A malformed timestamp is now visible three ways: the preserved field, an
  ingest counter, and normal queryability at roughly the right time — rather
  than as a `compaction_error` line repeating every 10s with no indication of
  which file or value caused it.
- Rows whose time came from the filename carry ingest time, not event time.
  This is the same accuracy the absent-timestamp path has always had.
