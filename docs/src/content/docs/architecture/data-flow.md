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
        decompress → parse JSON/ndjson → canonicalize into the envelope
          (capture _raw, resolve env/service/host/_time/severity — ADR-0009)
        → WalWriter::write() (atomic tmp→rename, ndjson, under wal/{env}/)
    → hot_buffer.insert(Arc<IngestBatch>)
    → bus.publish(Arc<IngestBatch>)
        └→ SSE subscribers (CompiledFilter, aho-corasick SIMD)
```

### WAL writer

Each ingest batch produces one WAL file per service, under the batch's env directory: `wal/{env}/{service}_{unix_millis}_{4_hex_random}.ndjson`. Writes are atomic via tmp-file-then-rename. The service name is carried **verbatim** — there is no filename sanitizer. It does not need one: `env` and `service` were validated at ingest (`[a-z0-9_-]{1,32}` and `[A-Za-z0-9._-]`, no dot-leading, ≤128 bytes), so path encoding is injective by validation and `api.v2` and `api_v2` stay distinct files (ADR-0009).

### Envelope canonicalization

Every accepted event is rewritten into the declared envelope — `_time`, `_ingested`, `_raw`, `_repairs`, `env`, `service`, `host`, `severity`, `severity_text`, `message` (ADR-0009). The canonicalizer captures `_raw` first (the client's string `_raw` verbatim when supplied, else the canonical pre-repair serialization), strips server-owned fields, consumes the `_time`/`timestamp`/`@timestamp` wire aliases, stamps `_ingested`, resolves `env` against the allowlist, fills `host` from the peer where that is honest, and derives the OTel `severity` number. The governing rule: **repair when the server has an honest answer, reject when it would guess.** Repairs are a closed code set recorded per-event in `_repairs` and counted by `trawl_ingest_repairs_total{code, service}`; rejections are per-event and carry a typed reason, so valid siblings in the same batch still land.

#### Timestamps

A present `_time` is valid iff it is a JSON string that, after trimming surrounding whitespace, parses as RFC 3339, as a date-time carrying an ISO 8601 *basic* offset (`+0530`, `+02` — what Java and Go encoders emit, and what RFC 3339 parsing alone rejects), as an offset-less date-time (read as UTC), or as a bare date (midnight UTC). Date and time may be separated by `T` or a space, the date may be `YYYY-MM-DD` or `YYYY/MM/DD`, and seconds and their fraction are optional (`HH:MM` is accepted). The grammar is deliberately not literal parity with DuckDB's `CAST` — anything outside it is repaired rather than guessed at. Valid values are canonicalized at ingest to RFC 3339 UTC at microsecond precision (DuckDB's native `TIMESTAMP` resolution), so the WAL and hot buffer only ever contain well-formed timestamps.

Anything else — an unparseable string, a nested object, a bare number — is **substituted, never fatal and never dropped**: `_time` is overwritten with the request-arrival time (the same default an absent `_time` gets) and the event is flagged `time.from_ingest`; the value as it arrived remains findable in `_raw`. A value that parses but is implausible (more than 10 years past, more than a day future) is kept as sent and flagged `time.out_of_range` — clock skew is a fact about the sender, not a reason to rewrite its data. The event still counts as accepted in both cases.

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
   - pins any new columns in the field catalog, then conforms the batch to the pins (below)
   - `UNION ALL BY NAME` with existing parquet (if any)
   - `COPY TO data/{env}/YYYY-MM-DD/HH/{service}.parquet` (atomic rename)
4. Drains the hot buffer entries for compacted batches (drain keys are `{env}/{wal-stem}`)
5. Deletes processed WAL files

### The field catalog: write-time type conformance

Every custom field gets a *pin* — a canonical type (`BIGINT`, `DOUBLE`, `TIMESTAMP`, `BOOLEAN`, `VARCHAR`) recorded in a postgres-backed catalog the first time a typed batch carries the field (an all-null batch defers). The invariant: **every parquet file trawl writes conforms to the catalog**, so `union_by_name` across any set of trawl-written files can never hit a type conflict. Pins are made durable in postgres — and mirrored into the in-process cache the query path reads — strictly *before* any parquet carrying them is published; if the pin write fails, no parquet is written and the WAL is retained for the next tick.

Conforming a batch `TRY_CAST`s each pinned column under a **lossless round-trip guard**: a cast only counts — in the pin ladder's ≥90% scoring and in the written value — when the value survives the round trip unchanged, because a bare `TRY_CAST` *rounds* rather than fails (`1.5` → `2`). A value that disagrees with its pin (or would be silently altered by the cast) becomes `NULL`, the conflict is recorded in `field_conflicts` (field, service, expected/observed type, rows nulled) and counted on `/metrics` (`trawl_catalog_conflicts_total`, `trawl_catalog_rows_nulled_total`), and the original value stays findable in `_raw`. Field names are ASCII-lowercased at ingest canonicalization (`field.name_case_folded` in `_repairs` when it changed something), so one DuckDB identifier has exactly one catalog spelling; nested objects/arrays are stringified to JSON text, so they pin `VARCHAR` and stay reachable via `json_extract_string`.

Because both sides of every merge are conformant, the compaction merge and the daily rollup run plain `UNION ALL BY NAME` with **no** cast-and-retry fallback: a conversion-class failure there can only mean a foreign file in the tree (dropped-in parquet, restore against a stale catalog), is logged as `catalog_invariant_violation`, and retries with the inputs retained — never a silent rewrite.

The catalog also keeps `field_services` (which services ever carried a field, first/last seen). It is ever-observed: retention deleting old partitions deliberately never reconciles it.

The catalog is bounded where field names are the client-chosen axis: at most 10,000 fields are ever pinned (a field arriving at a full catalog stays unpinned, so its column is not stored and its values remain in `_raw` — the denial warns as `catalog_pin_cap_reached` and counts on `trawl_catalog_pins_rejected_total`), and `field_conflicts` keeps a rolling window of the 100 newest rows *per field*, trimmed in the same transaction that writes. A sender that keeps disagreeing with a pin therefore costs a fixed amount of postgres, not a growing one; the exhaustive tally lives in the counters, which are never trimmed. `field_services` rows, by contrast, are **ever-observed**: nothing removes one — "which services ever carried this field" is historical fact, not an index over live files — and consumers window on `last_seen`.

A pin slot, unlike those windows, is spent permanently (pins are add-only until the repin rewrite), so *filling* the pin cap is its own hazard: every field pinned after the cap is reached is unstored on the whole install, not just for the sender that filled it. Compaction is therefore rationed — one batch may claim at most **half the free slots**, so no single ingest request can take the catalog and there is always headroom left for the next field a legitimate sender introduces. The fill level is exported as `trawl_catalog_pinned_fields` against `trawl_catalog_pin_capacity`: alert on the ratio, because `catalog_pin_cap_reached` only fires once the slots are already gone. The boot conformance pass is deliberately exempt from the ration — its proposals describe columns already on disk, and denying one of those *deletes* standing data instead of declining to add a column.

### Timestamp repair

The partition key is never hard-CAST in emitted SQL (ADR-0008) — one malformed value must never wedge a batch. Compaction resolves each row's `_time` (and `_ingested`, same ladder) through a three-arm `COALESCE`:

1. `TRY_CAST` of the raw value — always succeeds for post-canonicalization data;
2. the ingest instant recovered from the row's **own** WAL filename (`{service}_{unix_millis}_...`), read per-row via `read_json(..., filename=...)` — this drains WAL written before the ingest fix with no operator step;
3. the compaction instant — so no parquet row ever carries a NULL `_time` (a NULL partition key would sort first and fall outside every `last=` filter).

### Daily rollup

Once per day, hourly parquets are merged into a single daily parquet per service, sorted by `_time`. Crash-safe via `.rollup-{service}` marker files.

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

The envelope columns (`_time`, `_ingested`, `_raw`, `_repairs`, `env`, `service`, `host`, `severity`, `severity_text`, `message`) are declared and enforced at ingest; user fields beyond them stay dynamic in *name* — any field can appear at any time — but each name's *type* is pinned by the field catalog at first typed sight, so `union_by_name=true` reconciles heterogeneous column sets without ever facing a type conflict. `_time`/`_ingested` are converted to native `TIMESTAMP` during compaction (via the repair `COALESCE` above, never a hard CAST) for predicate pushdown.

### The epoch cutover

`data/EPOCH` (content `2`) marks the post-ADR-0009 layout; the legacy layout has none. At boot trawld runs a restartable, filesystem-only decision table: a fresh install creates the marked root; an epoch-2 root boots normally; a legacy root is renamed to `data.pre-schema-v2/` (an external `wal_dir` moves to `{wal_dir}.pre-schema-v2` with it) and a fresh marked root is created; a root with no marker AND an existing set-aside refuses to start with instructions. trawl never deletes the set-aside directory — remove it manually to reclaim disk. While it exists, disk-pressure retention is suppressed and logs `retention_disk_pressure_suppressed`: the set-aside sits outside `data/`, so deleting fresh partitions would destroy live data without reclaiming a byte of it. Age-based retention keeps running.

The rename only ever fires on evidence that trawl wrote the directory — a `wal/` subdir, a `YYYY-MM-DD` partition dir, or a parquet file — and only when `ingest.enabled` is true. A marker-less directory with none of those (a pre-created empty root, a fresh mount holding `lost+found`, a mistyped `[data] path`) is adopted in place: the marker is written, the root itself does not move. An *external* `wal_dir` is judged separately in that case, since no rename of the data root covers it: if it still holds pre-cutover flat `{wal_dir}/*.ndjson` files it is set aside as `{wal_dir}.pre-schema-v2` (`epoch_external_wal_set_aside`) — the compactor walks `{wal_dir}/{env}/` only, so leaving them would strand them forever, never compacted and never deleted. A WAL dir that is empty or already in the epoch-2 env layout is left alone. And an ingest-disabled node — a query-only trawld pointed at a shared or read-only parquet archive — is left completely untouched, marker included, since it owns nothing under that path.

One subtree survives the cutover: `scheduled/`, the report-run results. Those are materialized query results, not epoch-1 events, and each is named by a *relative* path in a live postgres `report_runs` row that the cutover deliberately does not touch — so the directory is carried back out of the set-aside into the fresh root (`epoch_report_runs_carried_over`) and every stored path keeps resolving. It is one rename, attempted on every boot that sees a set-aside, so a crash mid-cutover simply finishes the job on the next start; if both roots somehow hold report runs, trawld logs `epoch_report_runs_conflict` and leaves both alone rather than merging them for you.

### The boot conformance pass and the `CATALOG` marker

An ingest-enabled trawld proves the corpus conformant before serving queries. `data/CATALOG` holds the catalog's identity; when it is missing or disagrees with the postgres catalog (first boot after upgrade, a restored data root, a repointed `DATABASE_URL`), boot scans every parquet file under the data root — skipping `scheduled/`, which holds report-run results, not log events — seeds pins for unpinned columns by most-rows-wins across the corpus (a candidate's weight is the rows that actually carry a value for that column, `count(<col>)` — a mostly-NULL column in a huge file does not outvote a populated one in a small file), rewrites any file that disagrees with a pin (staged `.tmp` + atomic rename, conflicts recorded like compaction's), hydrates the in-process pin cache, and publishes the marker **last**. The pass is restartable: a crash before the marker republishes on the next boot and rewrites nothing that already conforms. Because it sits in front of HTTP serving, it is kept as close to metadata-only as the votes allow: describing a file reads its footer, but *counting* a column reads the column, and only unpinned fields vote — so pins are loaded before the scan and the count query covers only the columns that still need one, and is skipped entirely for a file whose every field is already pinned. A re-arm against a catalog that already pins the corpus (a lost marker, a restored pair of data root and database) is one footer read per file, not one corpus read. Both phases log a `catalog_conform_progress` heartbeat every 30s with `done`/`total`, and `catalog_conform_scan_start` reports the corpus size up front — a long pass is visibly working, and every rewrite is durable on its own, so a boot killed by a supervisor start timeout leaves the corpus strictly closer to conformant. A conformance failure is fatal on ingest nodes — an unproven corpus must not serve queries once the read-time safety nets are gone. This also means an ingest-node boot requires reachable postgres.

Individual bad files are not such a failure. A `.parquet` the pass cannot read — truncated by a disk-full event, bit-rotted, or simply not trawl's — is sniffed for parquet magic, skipped with a `catalog_conform_skip` warning and a `trawl_catalog_conform_skipped_total` bump, and left exactly where it is (unlike the rollup path, which must rename corrupt inputs aside or re-read them forever). One such file never keeps trawld down. It does stay outside the catalog invariant, so any query touching it errors — and since the corpus was not proven conformant, the marker is deliberately *not* published (`catalog_conform_incomplete`): every subsequent boot re-runs the pass until the file is repaired or removed.

**A query-only node checks the same marker, as a gate.** An ingest-disabled trawld runs no pass — it owns nothing under the data root — but it still serves `/api/v1/schema` from the catalog's pins, and pins from an unrelated catalog describe unrelated columns. So boot compares `data/CATALOG` against `catalog_state.catalog_id` and refuses to start when they disagree (`catalog_identity`): a fresh `trawl` database pointed at someone else's archive would otherwise advertise the seeded envelope while queries read entirely different physical columns. An archive holding no parquet at all passes (a cold start has no schema to get wrong); an archive whose tree cannot be fully enumerated does not, since an unread directory proves nothing. Fix it by pointing the app database at the catalog that owns the archive, or by booting once with `[ingest] enabled = true` so the pass adopts it.

**A readable file is not automatically trawl's file.** The rewrite is in place and lossy — values a `TRY_CAST` cannot read become NULL, the source is the destination, and there is no backup, dry-run, or operator opt-in — so the pass decides ownership from the *path*, before it opens anything: a file is adopted only when its path under the data root reads back as `{env}/{date}/{HH}/{service}.parquet` (or the daily rollup `{env}/{date}/{service}.parquet`) with every component passing the same injective predicates ingest enforces on names, and a real date/hour. Anything else — a parquet in an operator's own subtree, a reserved env name, a name ingest could never have written — is skipped exactly like an unreadable file: left byte-identical, never scanned, never voting on a pin, and holding back the marker. If you keep unrelated parquet under the data root, expect the pass to re-run every boot; move it out of the tree to let the pass complete.

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

All queries combine parquet sources with the hot buffer via `UNION ALL BY NAME`. The hot buffer snapshot is an atomic pair — the temp ndjson file plus the catalog pins intersected with the keys the snapshot's events actually carry, with the intersection recomputed on every snapshot call so a pin landing mid-compaction is reflected immediately. The emitter conforms the **hot branch only** to those pins: typed pins get `TRY_CAST` (a conflicting uncompacted value degrades to `NULL` for that row — the original stays in `_raw` — and the cold history remains fully visible), `VARCHAR` pins go through an untyped `json_extract_string(to_json(x), '$')` so strings land unquoted whatever type the snapshot's `read_json` inferred. Both of the hot side's TIMESTAMP columns (`_time`, `_ingested`) additionally keep their unconditional `TRY_CAST`s — a malformed hot value degrades that one row, never the whole union. The cold branch is deliberately plain: parquet is write-time conformant, and a defensive cold cast would mask a real invariant breach.

Field-name casing cannot split a hot column, because it is resolved before anything reaches the buffer: DuckDB identifiers are ASCII case-insensitive while JSON keys are not, so **every producer ASCII-lowercases field names at its own door** — HTTP ingest in the envelope canonicalizer (recorded as `field.name_case_folded`/`field.name_case_collision` in `_repairs`), the syslog listener when it constructs RFC 5424 structured-data keys (`sd_examplesdid@32473_eventid`), and internal telemetry when it collects tracing fields. One DuckDB identifier therefore has exactly one spelling in the hot buffer *and* in the catalog, the snapshot's pin intersection is a plain exact-name lookup, and the former read-side defences (the snapshot writer's case-variant merge, the pin cache's `VARCHAR` degrade for colliding spellings) are gone rather than dormant — the `VARCHAR` degrade in particular would have turned every numeric comparison on such a field lexical, permanently.

There is no read-time reconciliation beyond that: a type conflict surviving to execution means a corpus the catalog does not govern (foreign parquet dropped in post-boot, a restore against a stale catalog) and returns a loud error instead of a silently degraded result.

A hot-only fallback is permitted only when it cannot hide cold data: on a genuine cold start (the glob matches no parquet files) or a missing-column user error. Any other database failure with cold files present returns an error — a cold-data drop is never a silent HTTP 200 (ADR-0008).

## SSE streaming

A completely separate code path from SQL queries. `CompiledFilter` compiles the search stage of the DSL into an in-memory matcher using aho-corasick for text search and regex for glob patterns. Events are filtered against the broadcast channel, not DuckDB.

Bounded by an SSE semaphore (default: 32 concurrent streams). Back-pressure is communicated to clients via `StreamEvent::Lagged` events.
