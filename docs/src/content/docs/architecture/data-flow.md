---
title: Data Flow
description: How events move through trawl — from ingest to query.
---

## Ingestion pipeline

```
Vector (gzip JSON batch, 1MB / 5s)
  → POST /api/v1/ingest (rate limit: ingest_rpm per key, body: 16MB max)
    → auth middleware (argon2id, 5-min DashMap cache)
    → spawn_blocking:
        decompress → parse JSON/ndjson → validate service name
        → WalWriter::write() (atomic tmp→rename, ndjson)
    → hot_buffer.insert(Arc<IngestBatch>)
    → bus.publish(Arc<IngestBatch>)
        └→ SSE subscribers (CompiledFilter, aho-corasick SIMD)
```

### WAL writer

Each ingest batch produces one WAL file per service. Writes are atomic via tmp-file-then-rename. Filename format: `{service}_{unix_millis}_{4_hex_random}.ndjson`. Service names are validated against `[a-zA-Z0-9\-_.]` to prevent path traversal.

### Timestamp canonicalization

A present `timestamp` is valid iff it is a JSON string that, after trimming surrounding whitespace, parses as RFC 3339, as a date-time carrying an ISO 8601 *basic* offset (`+0530`, `+02` — what Java and Go encoders emit, and what RFC 3339 parsing alone rejects), as an offset-less date-time (read as UTC), or as a bare date (midnight UTC). Date and time may be separated by `T` or a space, the date may be `YYYY-MM-DD` or `YYYY/MM/DD`, and seconds and their fraction are optional (`HH:MM` is accepted). The grammar is deliberately not literal parity with DuckDB's `CAST` — anything outside it is preserved rather than guessed at. Valid values are canonicalized at ingest to RFC 3339 UTC at microsecond precision (DuckDB's native `TIMESTAMP` resolution), so the WAL and hot buffer only ever contain well-formed timestamps.

Anything else — an unparseable string, a nested object, a bare number — is **substituted, never fatal and never dropped**: `timestamp` is overwritten with the request-arrival time (the same default an absent timestamp gets) and the original value is preserved verbatim in `timestamp_invalid` (truncated to 256 chars). The event still counts as accepted; repairs are visible via the `trawl_ingest_events_repaired_total` counter and an `ingest_repairs` warn with sampled originals. `timestamp_invalid` is a sparse column, absent from the vast majority of parquet files.

### Hot buffer

An in-memory `RwLock<IndexMap>` keyed by batch ID, holding `Arc<IngestBatch>`. Events are inserted synchronously during ingest (not via an async consumer) to eliminate the query-visibility race — events are visible to queries immediately.

FIFO eviction at 100k events or 100MB. Generation-based snapshot caching ensures concurrent queries share a single temp ndjson file via `Arc<NamedTempFile>`.

Events stay in the hot buffer until drained after confirmed parquet write. Brief duplicates (visible in both hot buffer and parquet) are acceptable; invisible events are not.

### Event bus

A `tokio::sync::broadcast` channel (capacity 4096) publishing `Arc<IngestBatch>`. All subscribers share the same allocation. Lossy by design: slow consumers receive `RecvError::Lagged(n)` and never block the producer.

## Compaction

Every 10 seconds, the compaction task:

1. Scans each env directory (`wal/{env}/`) for `.ndjson` files older than 10 seconds
2. Groups them by service
3. For each service, spawns a blocking task with an ephemeral DuckDB connection:
   - `read_json_auto([wal files])` → repair `_time`/`_ingested` (below)
   - `UNION ALL BY NAME` with existing parquet (if any)
   - `COPY TO data/{env}/YYYY-MM-DD/HH/{service}.parquet` (atomic rename)
4. Drains the hot buffer entries for compacted batches (drain keys are `{env}/{wal-stem}`)
5. Deletes processed WAL files

### Timestamp repair

The partition key is never hard-CAST in emitted SQL (ADR-0008) — one malformed value must never wedge a batch. Compaction resolves each row's `_time` (and `_ingested`, same ladder) through a three-arm `COALESCE`:

1. `TRY_CAST` of the raw value — always succeeds for post-canonicalization data;
2. the ingest instant recovered from the row's **own** WAL filename (`{service}_{unix_millis}_...`), read per-row via `read_json(..., filename=...)` — this drains WAL written before the ingest fix with no operator step;
3. the compaction instant — so no parquet row ever carries a NULL `_time` (a NULL partition key would sort first and fall outside every `last=` filter).

### Daily rollup

Once per day, hourly parquets are merged into a single daily parquet per service, sorted by timestamp. Crash-safe via `.rollup-{service}` marker files.

### Retention

Age-based (default 90 days) + disk pressure (minimum 1 GiB free). The unit of deletion is a full date directory inside one env (`data/{env}/{date}/`) — per-env deletion is O(1) and never touches sibling envs. Disk-pressure candidates are merged oldest-first across envs.

## Storage layout

Two path dimensions besides time (ADR-0009): `env` outermost, `service` as the filename. Path encoding is injective by validation — both values are constrained at ingest and written verbatim, so `api.v2` and `api_v2` are distinct files and pruning is exact.

```
data/
  EPOCH                      # storage epoch marker, content "2"
  prod/
    2026-03-14/
      00/
        nginx.parquet        # hourly partition
        sshd.parquet
      01/
        nginx.parquet
      ...
      nginx.parquet          # daily rollup (merged hourly files)
      sshd.parquet
    2026-03-13/
      ...
  lab/
    2026-03-14/
      ...
```

The envelope columns (`_time`, `_ingested`, `_raw`, `_repairs`, `env`, `service`, `host`, `severity`, `severity_text`, `message`) are declared and enforced at ingest; user fields beyond them stay fully dynamic — `union_by_name=true` handles heterogeneous schemas across services. `_time`/`_ingested` are converted to native `TIMESTAMP` during compaction (via the repair `COALESCE` above, never a hard CAST) for predicate pushdown.

### The epoch cutover

`data/EPOCH` (content `2`) marks the post-ADR-0009 layout; the legacy layout has none. At boot trawld runs a restartable, filesystem-only decision table: a fresh install creates the marked root; an epoch-2 root boots normally; a legacy root is renamed to `data.pre-schema-v2/` (an external `wal_dir` moves to `{wal_dir}.pre-schema-v2` with it) and a fresh marked root is created; a root with no marker AND an existing set-aside refuses to start with instructions. trawl never deletes the set-aside directory — remove it manually to reclaim disk. While it exists, disk-pressure retention is suppressed and logs `retention_disk_pressure_suppressed`: the set-aside sits outside `data/`, so deleting fresh partitions would destroy live data without reclaiming a byte of it. Age-based retention keeps running.

The rename only ever fires on evidence that trawl wrote the directory — a `wal/` subdir, a `YYYY-MM-DD` partition dir, or a parquet file — and only when `ingest.enabled` is true. A marker-less directory with none of those (a pre-created empty root, a fresh mount holding `lost+found`, a mistyped `[data] path`) is adopted in place: the marker is written, the root itself does not move. An *external* `wal_dir` is judged separately in that case, since no rename of the data root covers it: if it still holds pre-cutover flat `{wal_dir}/*.ndjson` files it is set aside as `{wal_dir}.pre-schema-v2` (`epoch_external_wal_set_aside`) — the compactor walks `{wal_dir}/{env}/` only, so leaving them would strand them forever, never compacted and never deleted. A WAL dir that is empty or already in the epoch-2 env layout is left alone. And an ingest-disabled node — a query-only trawld pointed at a shared or read-only parquet archive — is left completely untouched, marker included, since it owns nothing under that path.

One subtree survives the cutover: `scheduled/`, the report-run results. Those are materialized query results, not epoch-1 events, and each is named by a *relative* path in a live postgres `report_runs` row that the cutover deliberately does not touch — so the directory is carried back out of the set-aside into the fresh root (`epoch_report_runs_carried_over`) and every stored path keeps resolving. It is one rename, attempted on every boot that sees a set-aside, so a crash mid-cutover simply finishes the job on the next start; if both roots somehow hold report runs, trawld logs `epoch_report_runs_conflict` and leaves both alone rather than merging them for you.

### App-state store

Log data is the parquet tree above; trawl's *app state* — query history, saved queries, schedules, and report run metadata — lives in a dedicated `trawl` postgres database. trawld migrates it automatically at boot and holds a session advisory lock for its lifetime (sole writer by design). Scheduled report *results* are written back into the data directory as parquet under `data/scheduled/{name}/run_{id}.parquet`, with only the relative path recorded in postgres.

## Query execution

```
DSL input
  → chumsky parser → AST
  → CTE-chained SQL emitter (parameterized)
  → DuckDB executor (semaphore-gated connection pool)
```

### Parser

Built with chumsky combinators. The search stage parses OR-separated groups of AND-joined tokens. Time filters are hoisted globally. A 65KB length guard prevents abuse.

### SQL emitter

Transforms the AST into CTE-chained DuckDB SQL. Each pipe stage boundary creates a new CTE (`_s0`, `_s1`, ...). All user values are parameterized via `?` placeholders except:
- Source paths — validated via allowlist
- PIVOT parameters — DuckDB limitation, inlined with quote-doubling

### Executor pool

A pool of N connections (default: `num_cpus`) sharing one in-memory DuckDB database via `try_clone()` (shared parquet metadata cache). Gated by a semaphore for concurrency control. Timeout via DuckDB's interrupt handle. Queries can be cancelled via `DELETE /api/v1/queries/{id}`.

### Source computation

`compute_source()` narrows the parquet glob by time filter + service filter using filesystem `stat()`. Adds 1-hour padding for compaction lag. Handles both hourly and daily partition layouts. When no parquet files exist, queries fall back to the hot buffer only.

### Hot buffer integration

All queries combine parquet sources with the hot buffer via `UNION ALL BY NAME`. The hot buffer snapshot is written to a temp ndjson file and read by DuckDB alongside the parquet files. This ensures freshly ingested events (not yet compacted) are always visible. The union applies `TRY_CAST` to the hot side's `timestamp` — a malformed hot value degrades that one row, never the whole union.

When the two sides disagree on a column's type, the union is retried with the conflicting columns cast to `VARCHAR` on both sides, preserving hot **and** cold rows. What counts as such a conflict is decided from evidence, not from the error text: DuckDB reports both an irreconcilable schema and an unconvertible value as `Conversion`-class errors, so the retry only fires when describing the two sources actually turns up a column they type differently. A genuine data-conversion error turns up none, is not misread as a schema conflict, and falls through to the policy below.

A hot-only fallback is permitted only when it cannot hide cold data: on a genuine cold start (the glob matches no parquet files) or a missing-column user error. Any other database failure with cold files present returns an error — a cold-data drop is never a silent HTTP 200 (ADR-0008).

## SSE streaming

A completely separate code path from SQL queries. `CompiledFilter` compiles the search stage of the DSL into an in-memory matcher using aho-corasick for text search and regex for glob patterns. Events are filtered against the broadcast channel, not DuckDB.

Bounded by an SSE semaphore (default: 32 concurrent streams). Back-pressure is communicated to clients via `StreamEvent::Lagged` events.
