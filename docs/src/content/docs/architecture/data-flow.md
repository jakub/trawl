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
          (capture _raw, resolve env/service/host, derive _time/_severity,
           stamp _producer — ADR-0009 as reshaped by ADR-0013)
        → WalWriter::write() (atomic tmp→rename, ndjson, under wal/{env}/)
    → hot_buffer.insert(Arc<IngestBatch>)
    → bus.publish(Arc<IngestBatch>)
        └→ SSE subscribers (CompiledFilter, aho-corasick SIMD)
```

### WAL writer

Each ingest batch produces one WAL file per service, under the batch's env directory: `wal/{env}/{service}_{unix_millis}_{4_hex_random}.ndjson`. Writes are atomic via tmp-file-then-rename. The service name is carried **verbatim** — there is no filename sanitizer. It does not need one: `env` and `service` were validated at ingest (`[a-z0-9_-]{1,32}` and `[A-Za-z0-9._-]`, no dot-leading, ≤128 bytes), so path encoding is injective by validation and `api.v2` and `api_v2` stay distinct files (ADR-0009).

### Envelope canonicalization

Every accepted event is rewritten into the declared ten-field envelope — `_time`, `_ingested`, `_raw`, `_repairs`, `_severity`, `_producer` (trawl-owned) plus `env`, `service`, `host`, `message` (sender-asserted) — per ADR-0009 as reshaped by ADR-0013. The canonicalizer captures `_raw` first (the client's string `_raw` verbatim when the door admits that proposal, else the canonical pre-repair serialization), strips the reserved `_` prefix off every non-proposable key so its value lands under the bare remainder, stamps `_ingested`, resolves `env` against the allowlist, fills `host` from the peer where that is honest, DERIVES `_time` and `_severity` from ordered source lists it only ever READS, and stamps `_producer` with the door the event actually came in at. Derivation never consumes: `timestamp`, `@timestamp`, `severity`, `severity_text` and `level` all stay as ordinary columns under the names their sender chose. The governing rule: **repair when the server has an honest answer, reject when it would guess** — and a derivation into the `_` namespace is an annotation, not a repair, so it confesses nothing. Repairs are a closed code set recorded per-event in `_repairs` and counted by `trawl_ingest_repairs_total{code, service}`; rejections are per-event and carry a typed reason, so valid siblings in the same batch still land.

#### Producer profiles

There are three ways an event can reach that canonicalizer, and all three go **through** it: the HTTP ingest handler, the syslog listener, and internal telemetry. They are *profiles*, not separate pipelines — a closed set (`http` | `syslog` | `trawld`) chosen by the server call site that owns the transport, never by anything on the wire. Every universal gate applies to every one of them: the ASCII field-name fold, the sealed `_`-prefix strip, the field-name length drop, nested stringification, the `_raw` cap and `_repairs` assembly.

What a profile *does* own is the identity it can prove and the derivation sources its transport licenses:

| Profile | `env` | `service` | `host` | Fixed derivation sources |
|---------|-------|-----------|--------|--------------------------|
| `http` | sender's, else `default_env` | sender's (rejects if unusable) | sender's, else the peer address | none |
| `syslog` | the configured env, boot-validated | source-IP map → valid APP-NAME → `default_service` | frame hostname → peer address → omitted | `syslog_severity` (dialect `syslog`), `syslog_timestamp` |
| `trawld` | the configured env, boot-validated | the literal `trawld` | resolved hostname, else omitted | none |

Identity is protected by **precedence**, not by a namespace: telemetry's own field names are ordinary sender vocabulary, and a payload key that collides with a slot its profile asserts simply loses, recorded as `field.producer_asserted`, with the displaced value still findable in `_raw`. An identical value is not a collision and earns no code, and neither is an explicit JSON `null` — that is absence, not a competing claim.

The two salvage doors cannot reject, because there is nobody to reject *to*: a syslog appliance and trawld's own tracing layer will not resend. So where the HTTP door refuses, they salvage and confess — an APP-NAME that fails the service charset lands under the profile's `default_service` with `service.from_profile`, and a hostname-less frame behind a configured `trusted_relays` peer keeps the event with `host` **omitted** and `host.omitted` recorded, rather than stamping the relay's address as the origin. Their env is boot-validated, so a per-event env failure is structurally impossible; if one somehow happens the event is dropped and counted on `trawl_ingest_profile_reject_total{profile, reason}`, whose whole closed matrix is published at zero — a flat series is the invariant holding.

`_producer` records which door admitted the event, so provenance is queryable data (`_producer=syslog | stats count()`). It is stamped by the server and unforgeable: a `_producer` on the wire takes the ordinary reserved-prefix strip and lands under a bare `producer`.

#### Syslog vocabulary

The listener writes no envelope field of its own. It publishes what it parsed as ordinary prefixed columns — `syslog_severity` (the **raw** PRI numeral, 0–7, omitted entirely when the frame carried no PRI), `syslog_timestamp` (the frame's own time in RFC 3339 UTC, omitted when the frame carried none), plus `syslog_facility`, `syslog_pid`, `syslog_msgid`, `syslog_source_ip` and the flattened `sd_*` structured-data pairs — and the profile's fixed derivation sources read those back. That is why `_severity` on a syslog event is inverted correctly while the stored numeral is not: provenance licenses the inversion, and the config is where provenance is asserted.

RFC 3164 timestamps carry no year. In the listener's local timezone, it evaluates the previous, current and next year and chooses the valid date nearest the event's arrival instant; an exact tie goes to the past. This keeps a late-December event received in early January in the previous year (and handles the reverse direction without landing months in the future). If none of those years can represent the date, such as a leap day surrounded by non-leap years, the timestamp remains unparseable and the normal `_time` derivation falls back to arrival time.

**Back-compat.** `_severity` on syslog events means exactly what it always did, so existing queries and alerts are unaffected. Rows written before this change simply lack the new columns: `_producer`, `syslog_severity` and `syslog_timestamp` read as `NULL` on them, since `UNION ALL BY NAME` tolerates an absent column. Nothing rewrites history — derivation is forward-only, and reinterpreting an old corpus is a repin, not a config reload.

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

Conforming a batch casts each pinned column under a **lossless round-trip guard**: a cast only counts — in the pin ladder's ≥90% scoring and in the written value — when the value survives the round trip unchanged, because a bare `TRY_CAST` *rounds* rather than fails (`1.5` → `2`) and reads a vocabulary it will not write back (`TRUE` → `true`). Two properties make that reading deterministic, and both are shared with the hot branch below through one expression builder (`trawl-core/src/conform.rs`, ADR-0011). First, **text first**: the cast applies to the column's canonical VARCHAR form, never to the type `read_json` inferred for the batch — JSON's cast domain is narrower than VARCHAR's for a fractional string and *wider* for a number under a `BOOLEAN` pin, so casting the inferred type would make one event's stored value a function of what else shared its batch. Second, the `TIMESTAMP` rung is **zone-aware**: it parses through `TIMESTAMPTZ`, so an offset in the text is applied and a zoneless text reads as UTC — which makes the session zone load-bearing, so every connection that conforms, scores the ladder, or reads a conformed hot branch runs `SET TimeZone='UTC'` first (the bundled DuckDB links ICU and otherwise defaults to the *host* zone). A value that disagrees with its pin (or would be silently altered by the cast) becomes `NULL`, the conflict is recorded in `field_conflicts` (field, service, expected/observed type, rows nulled) and counted on `/metrics` (`trawl_catalog_conflicts_total`, `trawl_catalog_rows_nulled_total`), and the original value stays findable in `_raw`. Field names are ASCII-lowercased at ingest canonicalization (`field.name_case_folded` in `_repairs` when it changed something), so one DuckDB identifier has exactly one catalog spelling; nested objects/arrays are stringified to JSON text, so they pin `VARCHAR` and stay reachable via `json_extract_string`.

Because both sides of every merge are conformant, the compaction merge and the daily rollup run plain `UNION ALL BY NAME` with **no** cast-and-retry fallback: a conversion-class failure there can only mean a foreign file in the tree (dropped-in parquet, restore against a stale catalog), is logged as `catalog_invariant_violation`, and retries with the inputs retained — never a silent rewrite.

The catalog also keeps `field_services` (which services ever carried a field, first/last seen, rows). It is ever-observed: retention deleting old partitions deliberately never reconciles it. Rows are written by compaction as batches land, and — for a corpus that predates the catalog — backfilled once by the boot conformance pass from the files it adopts, timestamped with each file's *partition hour* rather than boot time (stamping "now" over two-year-old data would park a dead field inside every `last_seen` window). That backfill carries its own completion flag, so an install that was conformed before the backfill existed re-arms the pass exactly once; unlike the best-effort conflict evidence, a failed backfill fails the pass rather than publishing the marker over a permanent gap.

Ongoing bookkeeping (conflict rows and observations) is written *after* the parquet is already durable, so it is retried on a momentary postgres blip but bounded by a wall-clock budget per batch and then abandoned with `catalog_bookkeeping_timeout` and one increment of `trawl_catalog_bookkeeping_timeouts_total`, labelled by which of the two writes (`conflicts`, `observations`) was in flight when the budget ran out. The trade is deliberate: a lost observation is a stale `last_seen`, while a compactor stalled behind a postgres outage stops the WAL draining until the hot buffer evicts — which is lost events.

The catalog is bounded where field names are the client-chosen axis: at most 10,000 fields are ever pinned (a field arriving at a full catalog stays unpinned, so its column is not stored and its values remain in `_raw` — the denial warns as `catalog_pin_cap_reached` and counts on `trawl_catalog_pins_rejected_total`), and `field_conflicts` keeps a rolling window of the 100 newest rows *per field*, trimmed in the same transaction that writes. A sender that keeps disagreeing with a pin therefore costs a fixed amount of postgres, not a growing one; the exhaustive tally lives in the counters, which are never trimmed. `field_services` rows, by contrast, are **ever-observed**: nothing removes one — "which services ever carried this field" is historical fact, not an index over live files — and consumers window on `last_seen`.

The evidence is also what lets trawl say a pin is *wrong*. Alongside the trimmed `field_conflicts` window, each `(field, service)` pair carries durable aggregates — first and last conflict, episode count, lifetime rows nulled — written in the same transaction, so a burst can evict the detail rows without erasing the history a verdict is judged on. Conflict rows additionally carry a small, byte-capped sample of the values the cast nulled, captured at null-time (the only moment they exist as values rather than `_raw` text to re-parse) and sanitised before storage.

The **analyzer** over that evidence is a read-time function plus one in-process cache — no daemon, no verdict table (ADR-0011 slice C). A pin is *degraded* when its evidence spans at least 24 hours **and** carries volume (100 rows shelved or 3 distinct episodes): the span alone would badge two rows lost a month apart, the volume alone would badge one bad deploy. Sender count is displayed evidence, never a gate — a homelab commonly has exactly one producer per field, and requiring two would make the badge unreachable where it matters most. The verdict is structured facts (since, senders, episodes, lifetime rows shelved, sample values, a suggested target type read from the misfit evidence: one uniform ladder rung, else `VARCHAR`); the words belong to whoever renders it. It rides `/api/v1/schema/fields` and `/schema/field`, counts on `trawl_catalog_degraded_fields`, and the schema-refresh tick loads the degraded set into memory so `/api/v1/query` can stamp `degraded_fields` — the fields a query *bound*, including ones it projected away — without the query path ever touching postgres. Freshness is bounded by one tick; the SSE stream carries no notice. The verdict is advisory and sender-influenceable by construction, which is exactly why acting on it is a human decision behind `schema_write`: `trawl schema repin`, never an automatic rewrite. The gate carries no recency term — evidence is never aged out, so fixing the sender does not by itself retire the badge, and should not: the rows the pin shelved are still missing. What retires it is the repin, which clears the field's evidence in the same transaction as the pin flip — including the same-type `--to <current> --force` resurrection-only pass, which recovers the shelved values from `_raw` without changing the pin at all.

A pin slot, unlike those windows, is spent permanently on the ingest path (only an operator-triggered repin rewrites one), so *filling* the pin cap is its own hazard: every field pinned after the cap is reached is unstored on the whole install, not just for the sender that filled it. Compaction is therefore rationed — one batch may claim at most **half the free slots**, so no single ingest request can take the catalog and there is always headroom left for the next field a legitimate sender introduces. The fill level is exported as `trawl_catalog_pinned_fields` against `trawl_catalog_pin_capacity`: alert on the ratio, because `catalog_pin_cap_reached` only fires once the slots are already gone. The boot conformance pass is deliberately exempt from the ration — its proposals describe columns already on disk, and denying one of those *deletes* standing data instead of declining to add a column.

### Repin: the shadow-generation rewrite

A wrong pin is not permanent: `POST /api/v1/schema/repin` (or `trawl
schema repin`) retypes one field's whole corpus, ADR-0011 slice B. The
job — one at a time install-wide, persisted in postgres — builds the new
generation as a **sibling** of the data root (`data.repin-next/`; a
sibling because DuckDB's recursive glob descends into dot-directories, so
an in-root staging dir would leak duplicate rows into fallback-glob
queries): unaffected and foreign files are hardlinked verbatim, affected
files (owned layout paths whose footer carries the column) are rewritten
through the same ConformPlan machinery compaction uses, with the target
column read as `COALESCE(guarded stored reading, guarded _raw
re-extraction)` — resurrection of conflict-shelved values, under the same
lossless guard, and the dry run counts with the very expression the
rewrite writes. A **symlink** under an env directory refuses the job
(dry run included): trawl never writes one, the shadow cannot carry it,
and skipping it would leave the swapped-aside original as the only copy
for the job's own cleanup sweep to delete — materialize it or move it
out of `data/{env}/` and retry. A data root that is itself a **mount
point** refuses the job for the same class of reason (dry run included,
before anything is built): the siblings would land on the parent
filesystem, where neither the hardlinks nor the swap's renames can reach
them — put the data root inside the volume, as both packaged layouts do.

Force on such a job names a number. Ingest keeps writing for the whole
build, so the finished shadow is never exactly the corpus the scan
measured; a forced request may state `max_nulled_rows` and
`max_ambiguous_rows`, and an unstated one resolves from that job's own scan
as `scan + max(scan / 10 rounded up, 10)`. One decision function answers
all three askers (the scan gate, the finished-shadow gate, and the job row
on the wire), so a dry run cannot promise an outcome the execution would
refuse, and a rewrite that came out worse than what force accepted refuses
the cutover with the accepted and actual counts named.

The badge over that evidence can also be acknowledged rather than repinned,
for the case where the fix is with the sender and the shelved rows have to
stay shelved: `POST /api/v1/schema/field/ack` records an episode
high-water, and the analyzer suppresses the verdict while the field's
episode count stays at or below it. A count rather than a timestamp,
because compaction can record several episodes inside one clock tick and a
time-keyed ack would suppress evidence nobody had seen. The next episode
re-raises the badge, the operator can withdraw the ack outright, and a
successful repin deletes it in the same transaction as the pin flip, since
the evidence it acknowledged no longer describes the pin.

An additive catch-up loop folds in files compaction writes meanwhile (the
file-relocating daily rollup is paused for the whole job, so the diff is
pure additions/replacements). The cutover is a few seconds under two
exclusion primitives — a corpus gate compaction batches take read-side,
and exclusivity over every query permit — because a half-swapped corpus
would not error: mixed scalar unions silently *promote* (probed by
execution), so exclusion is the whole atomicity budget. Inside it: final
increment, `data/REPIN` marker, two renames per env dir (`data/{env}` →
`data.repin-aside/{env}`, shadow → live; `wal/`, `scheduled/`, the
markers never move), then the pin flip (postgres + the in-process cache)
transactionally with the job's completion. WAL draining pauses only for
those seconds and the hot buffer keeps every undrained event queryable —
no event is ever invisible, and events ingested during the rewrite land
exactly once. A compaction batch that starts inside the pause is deferred,
not dropped: it waits at the corpus gate holding its WAL files and its hot
batch, and resumes itself the moment the pause lifts.

A crash anywhere is finished by boot: the marker replays through a
decision table *before* the epoch gate (building → abandon the disposable
shadow; cutover → complete the renames forward; cleanup → sweep), the
postgres half completes the idempotent flip and re-arms the boot
conformance pass to re-prove the corpus that same boot. Retention stands
down entirely while the marker or staging exists, and the job pre-flights
its double-held bytes against `min_free_disk_bytes`.

### Timestamp repair

The partition key is never hard-CAST in emitted SQL (ADR-0008) — one malformed value must never wedge a batch. Compaction resolves each row's `_time` (and `_ingested`, same ladder) through a three-arm `COALESCE`:

1. `TRY_CAST` of the raw value — always succeeds for post-canonicalization data;
2. the ingest instant recovered from the row's **own** WAL filename (`{service}_{unix_millis}_...`), read per-row via `read_json(..., filename=...)` — this drains WAL written before the ingest fix with no operator step;
3. the compaction instant — so no parquet row ever carries a NULL `_time` (a NULL partition key would sort first and fall outside every `last=` filter).

### Daily rollup

Once per day, hourly parquets are merged into a single daily parquet per service, sorted by `_time`. Crash-safe via `.rollup-{service}` marker files.

### Retention

Age-based (default 90 days) + disk pressure (minimum 1 GiB free). The unit of deletion is a full date directory inside one env (`data/{env}/{date}/`) — per-env deletion is O(1) and never touches sibling envs. Disk-pressure candidates are ranked across envs by expiry ratio: the directory that has used up the largest fraction of its env's age limit is deleted first.

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

The envelope columns (`_time`, `_ingested`, `_raw`, `_repairs`, `_severity`, `env`, `service`, `host`, `message`) are declared and enforced at ingest; user fields beyond them stay dynamic in *name* — any field can appear at any time — but each name's *type* is pinned by the field catalog at first typed sight, so `union_by_name=true` reconciles heterogeneous column sets without ever facing a type conflict. `_time`/`_ingested` are converted to native `TIMESTAMP` during compaction (via the repair `COALESCE` above, never a hard CAST) for predicate pushdown.

### The epoch cutover

`data/EPOCH` (content `2`) marks the post-ADR-0009 layout; the legacy layout has none. At boot trawld runs a restartable, filesystem-only decision table: a fresh install creates the marked root; an epoch-2 root boots normally; a legacy root is renamed to `data.pre-schema-v2/` (an external `wal_dir` moves to `{wal_dir}.pre-schema-v2` with it) and a fresh marked root is created; a root with no marker AND an existing set-aside refuses to start with instructions. trawl never deletes the set-aside directory — remove it manually to reclaim disk. While it exists, disk-pressure retention is suppressed and logs `retention_disk_pressure_suppressed`: the set-aside sits outside `data/`, so deleting fresh partitions would destroy live data without reclaiming a byte of it. Age-based retention keeps running.

The rename only ever fires on evidence that trawl wrote the directory — a `wal/` subdir, a `YYYY-MM-DD` partition dir, or a parquet file — and only when `ingest.enabled` is true. A marker-less directory with none of those (a pre-created empty root, a fresh mount holding `lost+found`, a mistyped `[data] path`) is adopted in place: the marker is written, the root itself does not move. An *external* `wal_dir` is judged separately in that case, since no rename of the data root covers it: if it still holds pre-cutover flat `{wal_dir}/*.ndjson` files it is set aside as `{wal_dir}.pre-schema-v2` (`epoch_external_wal_set_aside`) — the compactor walks `{wal_dir}/{env}/` only, so leaving them would strand them forever, never compacted and never deleted. A WAL dir that is empty or already in the epoch-2 env layout is left alone. And an ingest-disabled node — a query-only trawld pointed at a shared or read-only parquet archive — is left completely untouched, marker included, since it owns nothing under that path.

One subtree survives the cutover: `scheduled/`, the report-run results. Those are materialized query results, not epoch-1 events, and each is named by a *relative* path in a live postgres `report_runs` row that the cutover deliberately does not touch — so the directory is carried back out of the set-aside into the fresh root (`epoch_report_runs_carried_over`) and every stored path keeps resolving. It is one rename, attempted on every boot that sees a set-aside, so a crash mid-cutover simply finishes the job on the next start; if both roots somehow hold report runs, trawld logs `epoch_report_runs_conflict` and leaves both alone rather than merging them for you.

### The boot conformance pass and the `CATALOG` marker

An ingest-enabled trawld proves the corpus conformant before serving queries. `data/CATALOG` holds the catalog's identity; when it is missing or disagrees with the postgres catalog (first boot after upgrade, a restored data root, a repointed `DATABASE_URL`), boot scans every parquet file under the data root — skipping `scheduled/`, which holds report-run results, not log events — seeds pins for unpinned columns by most-rows-wins across the corpus (a candidate's weight is the rows that actually carry a value for that column, `count(<col>)` — a mostly-NULL column in a huge file does not outvote a populated one in a small file), rewrites any file that disagrees with a pin (staged `.tmp` + atomic rename, conflicts recorded like compaction's), hydrates the in-process pin cache, and publishes the marker **last**. The pass is restartable: a crash before the marker republishes on the next boot and rewrites nothing that already conforms. Because it sits in front of HTTP serving, it is kept as close to metadata-only as the votes allow: describing a file reads its footer, but *counting* a column reads the column, and only unpinned fields vote — so pins are loaded before the scan and the count query covers only the columns that still need one, and is skipped entirely for a file whose every field is already pinned. A re-arm against a catalog that already pins the corpus (a lost marker, a restored pair of data root and database) is one footer read per file, not one corpus read. Both phases log a `catalog_conform_progress` heartbeat every 30s with `done`/`total`, and `catalog_conform_scan_start` reports the corpus size up front — a long pass is visibly working, and every rewrite is durable on its own, so a boot killed by a supervisor start timeout leaves the corpus strictly closer to conformant. A conformance failure is fatal on ingest nodes — an unproven corpus must not serve queries once the read-time safety nets are gone. This also means an ingest-node boot requires reachable postgres.

Individual bad files are not such a failure. A `.parquet` the pass cannot read — truncated by a disk-full event, bit-rotted, or simply not trawl's — is sniffed for parquet magic, skipped with a `catalog_conform_skip` warning and a `trawl_catalog_conform_skipped_total` bump, and left exactly where it is (unlike the rollup path, which must rename corrupt inputs aside or re-read them forever). One such file never keeps trawld down. It does stay outside the catalog invariant, so any query touching it errors — and since the corpus was not proven conformant, the marker is deliberately *not* published (`catalog_conform_incomplete`): every subsequent boot re-runs the pass until the file is repaired or removed.

**A query-only node checks the same marker, as a gate.** An ingest-disabled trawld runs no pass — it owns nothing under the data root — but it still serves `/api/v1/schema` from the catalog's pins, and pins from an unrelated catalog describe unrelated columns. So boot compares `data/CATALOG` against `catalog_state.catalog_id` and refuses to start when the marker names a *different* catalog: a repointed `DATABASE_URL` over someone else's archive would otherwise advertise the seeded envelope while queries read entirely different physical columns. Fix that by pointing the app database at the catalog that owns the archive, or by booting once with `[ingest] enabled = true` so the pass adopts it. The refusal stops there. An archive holding no parquet at all passes (a cold start has no schema to get wrong), and so does an archive with **no marker**: that is exactly what an incomplete pass leaves behind (see above — one skipped export file is enough), and an ingest node warns and serves it, so the query-only node logs `catalog_identity_unproven` and serves it too rather than turning "disable ingest and restart to investigate" into a startup failure.

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

`compute_source()` narrows the parquet glob by time filter + service filter using filesystem `stat()`. Adds 1-hour padding for compaction lag. Handles both hourly and daily partition layouts. When no parquet files exist, queries fall back to the hot buffer only. A time filter turns the source into a *list* — one glob per hour in range — and the planner drops elements whose parent directory does not exist. That prune is advisory only, and deliberately weak: hour directories are created by whichever service compacted into them first, so an existing parent proves nothing about the service being queried.

Per-element liveness has exactly one authority, one level down: **the executor resolves a list source against the filesystem once, before the read** — and *after* the hot-buffer snapshot, so a file published mid-query cannot fall out of both halves of the union — narrowing it to the elements that reach a file. A literal element is answered by a single `stat()`; a pattern goes through DuckDB's `glob()`. `read_parquet` rejects a whole list when a *single* element matches nothing, so an unresolved list is unreadable whenever any hour in range holds no file of its own — the everyday shape for a sparse-traffic service. Resolution happens on every read lane, and the evidence it gathers (did anything match?) is what the outcome policy below consults, rather than globbing a second time after a failure: a file that was there at resolution and gone at read time is a race, and must not be quietly re-resolved away.

### Hot buffer integration

All queries combine parquet sources with the hot buffer via `UNION ALL BY NAME`. The hot buffer snapshot is an atomic pair — the temp ndjson file plus the catalog pins intersected with the keys the snapshot's events actually carry, with the intersection recomputed on every snapshot call so a pin landing mid-compaction is reflected immediately. The emitter conforms the **hot branch only** to those pins, through the *same* guarded, text-first expression compaction writes with (`trawl-core/src/conform.rs`): the column's text form is `json_extract_string(to_json(x), '$')` in **both** lanes — that spelling unquotes strings whatever type the snapshot's `read_json` inferred, and the emitter has no `DESCRIBE` to choose anything else — and each typed pin's guarded cast applies to that text. Compaction does not pick its own spelling either, though its `DESCRIBE` would let it: under a VARCHAR pin the guard is the identity, so the text form *is* the stored value (`CAST(col AS VARCHAR)` renders a DOUBLE `1e20` as `1e+20` where `to_json` renders it `100000000000000000000.0`), and a per-lane spelling would be a per-lane corpus. A hot value that does not conform degrades to `NULL` for that row (the original stays in `_raw`, the cold history remains fully visible), which is also what keeps it from throwing the union. Sharing one expression is the point: a value that read `true` while it was hot and `NULL` once it compacted would be a query whose answer changes with a background timer. Both of the hot side's TIMESTAMP columns (`_time`, `_ingested`) additionally keep their unconditional `TRY_CAST`s — they need none of the zone-aware rung's work, since ingest already canonicalized them to UTC — a malformed hot value degrades that one row, never the whole union. The cold branch is deliberately plain: parquet is write-time conformant, and a defensive cold cast would mask a real invariant breach.

Field-name casing cannot split a hot column, because it is resolved before anything reaches the buffer: DuckDB identifiers are ASCII case-insensitive while JSON keys are not, so **field names are ASCII-lowercased at the door** — the one envelope canonicalizer every producer profile enters through, recorded as `field.name_case_folded`/`field.name_case_collision` in `_repairs`. That covers the RFC 5424 structured-data keys the syslog listener builds (`sd_exampleSDID@32473_eventID` → `sd_examplesdid@32473_eventid`) and the mixed-case tracing field names internal telemetry collects (`myField` → `myfield`), which used to be folded by two further hand-rolled rules. One DuckDB identifier therefore has exactly one spelling in the hot buffer *and* in the catalog, the snapshot's pin intersection is a plain exact-name lookup, and the former read-side defences (the snapshot writer's case-variant merge, the pin cache's `VARCHAR` degrade for colliding spellings) are gone rather than dormant — the `VARCHAR` degrade in particular would have turned every numeric comparison on such a field lexical, permanently.

There is no read-time reconciliation beyond that: a type conflict surviving to execution means a corpus the catalog does not govern (foreign parquet dropped in post-boot, a restore against a stale catalog) and returns a loud error instead of a silently degraded result.

A hot-only fallback is permitted only when it cannot hide cold data: on a genuine cold start (the source reaches no parquet files) or a missing-column user error. Any other database failure with cold files present returns an error — a cold-data drop is never a silent HTTP 200 (ADR-0008).

**The gate is lane-independent.** All four read lanes — query and parquet export, each with and without a hot buffer — classify their outcome the same way and route it through the same decision table, so a query running with an empty hot buffer answers exactly as one running with a full one. That matters more than it sounds: an empty hot buffer takes the *no-hot* code path, which is the state any install reaches after an idle minute. A read that answers "no files" while its source still reaches files on disk returns a retryable `503 cold_data_unread` on every lane, never an empty success. A source that genuinely reaches nothing is the one shape where an empty answer is the truth: a query returns zero rows, and an export surfaces the underlying error, because there is no empty result for it to write.

## Scheduled reports

A saved query plus a schedule is a report: trawld runs the query on a
fixed interval and stores each result. What each run *covers* is the
schedule's business, not the query text's (ADR-0018). A nightly schedule
over a saved `last=2h` used to report 2 of every 24 hours, and a run that
started late reported a different 2 hours than the one before it.

### Window modes

A schedule has one of three modes.

`window = "since_last"` tiles. Each run covers `[the previous run's window
end, this fire - lag)`, so consecutive runs cover consecutive intervals
with no gap and no overlap. This is the mode for anything you intend to
add up across runs.

`window = "<duration>"` is a fixed trailing span, re-measured from every
fire: a run at 03:00 with `window = "2h"` covers `[01:00, 03:00)`
regardless of what the last run did. It keeps no watermark, so a missed
fire is simply a missing run and nothing heals it. A span that differs
from the interval is allowed and sometimes wanted: `interval = "1h",
window = "2h"` gives every report an hour of overlap with its predecessor,
which is how you write a rolling two-hour view; `interval = "1h", window =
"30m"` samples half of each hour on purpose. Neither is a mistake trawl
should correct, so it does not.

No `window` is query mode, the shape schedules had before this: the saved
DSL executes verbatim, time clause and all, and the run records no bounds.

`lag` shifts both bounds back on either mode. With `lag = "5m"` the 03:00
run covers up to 02:55, not 03:00, so an event stamped 02:54 that only
reached the corpus at 02:58 is still inside the window that counts it. The
axis is `_time`, the sender's own timestamp, which is what makes a report
agree with an interactive query over the same interval and lets the
partition layout prune the read. `lag` is the allowance for that choice.

### Planned boundaries

The scheduler fires on a planned cursor, `next_fire_at`, not on elapsed
time since the last run started. A run that takes 90 seconds, a poll that
lands 8 seconds late, a restart: none of them move the cursor. Each tick
samples one instant, and every schedule in that tick is judged against it.

When several boundaries have passed (the daemon was down, or the previous
run was still going), the tick takes the *latest* boundary at or before
now and moves the cursor one interval past it. Missed fires never replay
as N runs.

### The watermark

A `since_last` schedule keeps `covered_through`, the end of the newest
window a successful run covered. It advances only on success, only for a
run claimed in `since_last` mode, and only when that run's end is later
than what is already covered, so a slow run finishing after a later one
cannot rewind coverage. A failed or timed-out run leaves it alone, and the
next success covers its own interval and the failed one in a single
window.

It is seeded when the schedule is created, at `next_fire_at - interval -
lag`, which is the origin of what the schedule owes. Seeding matters for
exactly one case: if the *first* run fails, an unset watermark would send
the next run back to "one interval ending at my own fire" and the failed
interval would be dropped with nothing recording the loss.

### Catch-up and the clamp

Coalescing is unbounded by default, and a week of downtime would otherwise
hand the next run a week-wide window and one enormous query. So a gap
wider than `max_catchup_intervals` intervals (config, default 24) clamps
the window start forward to `window_end - max_catchup_intervals *
interval`. The run then carries `window_truncated: true` and increments
`trawl_scheduler_window_truncated_total`.

Alert on that counter. A truncated run is the one case where coverage is
permanently missing from the report series, and the run row is the only
place that says so.

### A worked example

Hourly schedule, `window = "since_last"`, `lag = "5m"`, created at
2026-03-14T02:00:00Z. Creation seeds `covered_through` to 00:55 and sets
`next_fire_at` to 02:00.

| fire | window | outcome | `covered_through` after |
|------|--------|---------|-------------------------|
| 03-14 02:00 | `[00:55, 01:55)` | success | 01:55 |
| 03-14 03:00 | `[01:55, 02:55)` | success | 02:55 |
| 03-14 04:00 | `[02:55, 03:55)` | error | 02:55, unchanged |
| 03-14 05:00 | `[02:55, 04:55)` | success | 04:55 |
| 03-16 09:00 | `[03-15 08:55, 03-16 08:55)`, truncated | success | 03-16 08:55 |

The 05:00 run is the healing one: two intervals in a single window,
because 04:00 failed and left the watermark standing.

Then trawld is down from 03-14 05:30 until 03-16 09:03. The tick at 09:03
takes 09:00 as its boundary (the fires at 06:00, 07:00, 08:00 on the 14th
and every fire on the 15th are folded in, not replayed), and asks for
`[03-14 04:55, 03-16 08:55)`. That is 52 hours against a bound of 24, so
the start is clamped to 03-15 08:55 and the run is flagged. The 28 hours
from 03-14 04:55 to 03-15 08:55 are not in any report and will not be.
They are still in the corpus: query them interactively with
`earliest=`/`latest=`.

The 03:00 run's stored `query` is the saved DSL with
`earliest="2026-03-14T01:55:00.000000Z" latest="2026-03-14T02:55:00.000000Z" `
in front of it. Paste it into `trawl query` and you get that report back.

### Editing a schedule

Changing the interval or the window re-anchors `next_fire_at` to now: the
cadence you asked for starts from the edit. Changing `max_runs` or
flipping `enabled` leaves the cursor alone, so repeated edits cannot keep
a schedule permanently un-due.

An existing `covered_through` is never cleared by an edit, to the schedule
or to the saved DSL. The per-run resolved text is the audit trail, and a
watermark reset would silently re-report or skip coverage. An *absent* one
is seeded at the new origin when the edit re-anchors a `since_last`
schedule, for the reason seeding exists at creation. If an edit leaves the
watermark at or past the window a fire would cover, the tick advances the
cursor and runs nothing rather than asking the same question every poll.

Deleting the schedule (or the saved query above it) takes the watermark
and every run row with it, and the result files those rows pointed at are
unlinked after the transaction commits. Recreating the schedule starts a
fresh origin, not the old coverage.

## SSE streaming

A completely separate code path from SQL queries. `CompiledFilter` compiles the search stage of the DSL into an in-memory matcher using aho-corasick for text search and regex for glob patterns. Events are filtered against the broadcast channel, not DuckDB.

Separate path, identical answer: the filter is compiled with the field catalog's pin snapshot and applies the same comparison rules the SQL emitter does (one rule table, two consumers — ADR-0011 slice A), and it evaluates in SQL's three-valued logic, so a missing field is *unknown* rather than false and `NOT` cannot invert it into a match. The snapshot is taken when the stream opens and held for its life — a repin takes effect on reconnect. See the [DSL reference](/reference/dsl/) for the operator-facing rules.

Bounded by an SSE semaphore (default: 32 concurrent streams). Back-pressure is communicated to clients via `StreamEvent::Lagged` events.

## Internal telemetry

When `[ingest] internal_telemetry` is enabled (the default), a custom tracing layer inside `trawld` turns the daemon's own structured events into ordinary `service=trawld` records: canonicalized through the one envelope door under the `trawld` profile, serialized to ndjson, written durably through the ingest WAL, inserted into the hot buffer, published to the event bus, and eventually compacted to parquet like any other service. Its tracing fields are ordinary sender vocabulary with no reserved prefix — they get the same fold, the same sealed-namespace strip and the same 255-byte name cap every other producer's fields get, and a field name that collides with `service`, `env` or `host` loses to the profile's assertion rather than misfiling the daemon's own logs.

**Durability before visibility.** Each flush cycle (default 1s) stages the buffered events as one batch and writes it to the WAL on Tokio's blocking pool — the fsync barriers never run on an async executor worker. Only after the WAL write succeeds is the batch inserted into the hot buffer and published to SSE, exactly once; queries can never observe telemetry that would disappear after a restart.

**Bounded retry, accounted loss.** An ordinary failed WAL write retains its batch on a FIFO retry queue (reported via rate-limited stderr and `trawl_telemetry_wal_write_failures_total`) — a transient storage error loses nothing. A panicked or cancelled blocking write has consumed its batch, so the unrecoverable events and ndjson bytes are counted exactly once under drop reason `write_crashed`, alongside the existing single write-failure increment. Once a write succeeds again, the rest of the queue drains oldest-first in *coalesced* units of at most 4 MiB of ndjson — one WAL file (and one `batch_id`) per unit — so recovering from an hours-long outage costs writes proportional to the queued bytes rather than one fsynced file per flush tick it lasted. Nothing merges while the volume is still failing, so shedding under the cap keeps its per-tick granularity. Retained memory is capped by `[ingest] telemetry_buffer_max_bytes` (default 16 MiB, an estimated charge like the hot buffer's) — ONE budget over the active buffer, the queue and the batch in flight through a write, enforced as events arrive rather than after staging, because a wedged `spawn_blocking` write otherwise leaves the active buffer growing unbounded. Over budget, the oldest queued batches are shed first and then the incoming event itself is dropped (nothing is exempt), with exact event/byte accounting in `trawl_telemetry_{events,bytes}_dropped_total{reason="buffer_cap"}` (`reason="preinit_cap"` covers the bootstrap buffer before the WAL writer exists; `reason="write_crashed"` covers a consumed task). `trawl_telemetry_buffer_{events,bytes}` gauge that whole charge — nonzero across scrapes warns *before* loss begins, and all of these series are Prometheus-scrapeable precisely while self-ingestion is unavailable. After recovery, a searchable `telemetry_dropped` event records what was lost. Graceful shutdown attempts a final flush under a wall-clock budget so an unhealthy volume cannot hang the daemon: the periodic flush is raced against the shutdown signal (a wedged fsync cannot stop the task observing it), the final drain is capped at 5s, and process exit is capped at 10s — a frozen volume costs at most one lingering blocking thread, never a stalled restart.

**What internal telemetry covers — and what it does not.** `service=trawld` is the *daemon's* self-observation, not a deployment-wide or transactional audit trail:

- **trawld only.** `trawl-web` and the CLIs (`trawl`, `trawl-admin`, `fleet-admin`) log to stdout/stderr; capturing those is the deployment's job (journald, container logs).
- **Fleet mutations are observed by polling.** The key-audit task snapshots keystore state at startup and emits diffs every `audit_interval_secs` (default 30s): events are *eventual*, multiple changes inside one interval coalesce, and a create-then-delete entirely between polls is missed. It is a monitoring aid, not a transactional mutation ledger — a real Fleet audit table/outbox would be a separate security feature.
- **Query lifecycle events carry metadata, not query text.** `query_start`/`query_complete`/`query_timeout`/`query_failed`, `export_start`/`export_complete`, and `stream_start` share a `query_id` and carry actor, roles, outcome, timing, pagination, row counts, and `query_len` — never the raw DSL. Failures carry a stable `error_class` (`parse`, `emit`, `database`, `timeout`, …) rather than an error message, because parser and emitter text quotes the user's own tokens and a database error embeds the generated SQL. Full text lives in authenticated query history, the opt-in [query debug log](/reference/configuration/#the-query-debug-log), and the DEBUG-only `query_text` / `query_error_text` events that the default filter never stores.
- **No OTLP export.** There is no OpenTelemetry trace/log export; HTTP request correlation comes from trawld's own spans, and metrics are Prometheus-only.
- **Unmetered rejections are stdout-only.** The `fleet_auth`, `auth.backend`, `preauth.transport` and `trawl_server::policy::unmetered` targets are excluded from the corpus regardless of `RUST_LOG`: the bearer middleware runs before per-key rate limiting (the limiter needs a verified key), the accept loop's `tls_handshake_failed` warning fires before there is even a TLS session — a bare TCP connect-and-close is enough — and the policy layer's grantless 403 is deliberately decided outside the limiter too, so an authenticated key with zero trawl permissions (a shared-keystore neighbour's key) never spends a bucket to be told no. Persisting any of them would make a connection or request flood no rate limit can slow a durable-write amplifier. They stay on stdout; the corpus keeps everything trawld emits behind the limiter, the `storage.backend` alarm target, and the catalog/health events an outage also produces. Exclusion from the corpus is not loss of the signal: every rejection is counted on `/metrics` as `trawl_auth_failures_total{reason}` — `unauthorized` (missing, malformed, invalid, revoked or expired token), `backend_unavailable` (the keystore failed to answer), `no_trawl_grant` (a verified key with no usable trawl permission), plus defensive `forbidden` / `internal`. The label set is closed and carries no key, name or path, so the series count is fixed however hard an unauthenticated client hammers the endpoint — credential stuffing, token brute force and a revoked key still in use stay alarmable without handing anyone a durable-write lever.

Events pass the [`RUST_LOG` filter](/reference/configuration/#logging-filter-rust_log) stdout does, minus those pre-authn targets; pre-tracing failures (config file errors) surface only on stderr.
