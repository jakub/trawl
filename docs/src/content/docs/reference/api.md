---
title: HTTP API
description: REST API reference for trawld.
---

The trawl server exposes a REST API over HTTPS. All routes under `/api/v1` except `/health` and `/ingest` require bearer token authentication.

## Authentication

Include your API token in the `Authorization` header:

```
Authorization: Bearer flt_your_token_here
```

API keys are managed with `fleet-admin` against the fleet postgres
keystore (`DATABASE_URL`):

```bash
fleet-admin keys create --name "my-key" --kind human --role trawl-analyst
fleet-admin keys list
fleet-admin keys revoke <key-prefix>
```

### Roles and permissions

Roles are data-defined (ADR-0006): named, cross-app bundles of permission
strings stored in the fleet keystore, managed with `fleet-admin roles`.
A key holds any number of roles; effective permissions are the union.
The migration converts the former static tiers into these roles:

| Role | Trawl permissions |
|------|------------|
| `trawl-admin` | `query`, `schema_read`, `validate`, `saved_query`, `export`, `stream`, `query_cancel`, `server_manage` |
| `trawl-analyst` | `query`, `schema_read`, `validate`, `saved_query`, `export`, `stream`, `query_cancel` |
| `trawl-reader` | `query`, `schema_read`, `query_cancel` |
| `trawl-ingest` | `ingest` |

Handlers gate on permissions, never role names — reshape the tiers with
`fleet-admin roles` without a deploy. A valid key that lacks the
permission a route asks for is refused **401** (the whole per-route gate
answers alike); **403** is reserved for the grant gate in front of it — a
key resolving *no* recognized trawl permission at all.

One permission exists outside the converted tiers: `schema_write` gates
the repin trigger (the first data-mutating schema action) and is
deliberately granted to **no** role by default — a schema-admin role is
one `fleet-admin roles create` away:

```bash
fleet-admin roles create --name trawl-schema-admin \
  --perm trawl:schema_read --perm trawl:schema_write
fleet-admin keys assign-role <key-prefix> trawl-schema-admin
```

No `server_manage` rides along, and no deploy is involved: the migration
only registers `trawl:schema_write` in the permission vocabulary.

## Endpoints

### Health

```
GET /api/v1/health
```

Unauthenticated. Returns server health status.

### Query

```
POST /api/v1/query
Content-Type: application/json

{
  "query": "_severity>=error last=1h | stats count() by service"
}
```

Execute a DSL query. Returns results as a JSON array of row objects.

The response may also carry `degraded_fields` — the fields the query
**bound** (filters, `where`/`let` expressions, group-by and sort keys)
whose catalog pin the analyzer currently calls degraded, meaning results
may be missing values that pin shelved. Fields the query filtered on and
then projected away are included: that is exactly the incomplete case.
The key is **absent when empty**, which is the ordinary case. Freshness is
bounded by one schema-refresh tick (`schema_cache_ttl_secs`, default 60s).
The SSE stream carries no notice, and embedded `--data` mode has no
catalog to consult. `trawl query -f table` prints it as a footer line and
the [web UI](/reference/web-ui/#the-incomplete-results-notice) renders it
as a dismissible notice over the results.

```json
{ "columns": [], "rows": [], "truncated": false,
  "pagination": { "limit": 1000, "offset": 0, "returned": 12 },
  "degraded_fields": ["duration"] }
```

### Validate

```
POST /api/v1/validate
Content-Type: application/json

{
  "query": "_severity>=error | stats count() by host"
}
```

Validate DSL syntax without executing.

### Schema

```
GET /api/v1/schema
GET /api/v1/schema?service=nginx
GET /api/v1/schema?all=true
```

Returns column names and types, served from the **field catalog** — the
write-time type authority — never a parquet `DESCRIBE`. Corpus facts
(dates, sizes, services, file count) come from a TTL-cached filesystem
walk; `cached` reports whether *they* were cached. The column set is
cached under the same TTL (`schema_cache_ttl_secs`) when the request is
unscoped, so a newly pinned field can take up to that long to appear; a
`?service=` request is always served fresh from the catalog.

Parameters:

| Parameter | Description |
|-----------|-------------|
| `service` | Only fields that service has carried. |
| `all` | `true` lifts the retention window: by default a field whose most recent observation predates the retention horizon is hidden. The horizon is the longest effective age across envs, the global `[retention] max_age_days` (default 90) against every `[retention.env.<name>]` override, and a `0` anywhere in that set means no window at all. A field with no observations at all (e.g. the envelope on a fresh install) is always shown. |

Observations come from compaction, and — for a corpus that predates the
catalog — from the boot conformance pass, which backfills them from the
files it adopts (timestamps taken from each file's partition hour, not from
boot time). So `?service=` and the window answer for historical data too;
a corpus older than the horizon that retention has not yet pruned needs
`?all=true` to list its fields.

```
GET /api/v1/schema/fields
GET /api/v1/schema/fields?service=nginx&since_secs=604800&limit=100
```

Lists pinned fields with aggregated evidence: type, pin provenance
(`pinned_from`, `pinned_at`), per-field service count, cumulative rows,
first/last observation, and conflict counts. The response carries
`pinned_total` / `pin_capacity` (catalog fill) and `truncated` (the
default limit is 500, clamped to the pin cap).

A field whose pin is doing sustained damage also carries a `verdict`
object (absent otherwise — there is no null verdict):

```json
{ "name": "duration", "type": "BIGINT", "conflict_count": 12,
  "rows_nulled": 34,
  "verdict": { "since": "2026-08-01T10:00:00.000000Z", "services": 2,
               "episodes": 41, "rows_shelved": 1290,
               "samples": ["n/a", "pending"], "suggested_to": "VARCHAR" } }
```

Degraded means the conflict evidence **spans at least 24 hours** and
carries volume — 100 rows shelved or 3 distinct episodes. Sender count is
displayed evidence, never a gate: one producer sending `status="accepted"`
for a week is exactly the case worth surfacing. The verdict is structured
facts only; the words are the client's.

Two numbers that look alike and are not: `rows_nulled` sums the conflict
rows still inside the per-field recency window, while `rows_shelved` is
the **lifetime** total from durable aggregates the window cannot evict.
`samples` are a bounded, de-duplicated set of the values the pin nulled —
a sample, never a manifest; every one of them remains in `_raw`.

The verdict is advisory and sender-influenceable by construction: acting
on it means `trawl schema repin`, which needs `schema_write` and a human.
`trawl_catalog_degraded_fields` gauges how many pins currently qualify.

`?service=` scopes the numbers, not just the row set: `service_count`,
`row_count`, `first_seen`, `last_seen` and the `?since_secs=` window all
describe that one service's observations, so a field another service is
still sending does not keep showing up under a service that stopped. The
same holds for `?service=` on `/api/v1/schema` above. The `verdict` is the
exception, deliberately: a pin is global, so its verdict is always
install-wide — under `?service=` the observation numbers describe that
service while the verdict beside them describes the field.

```
GET /api/v1/schema/field?name=duration
GET /api/v1/schema/field?name=duration&limit=500&after=<services_cursor>
```

One field's detail: the pin, one page of per-service observations, and
retained conflict evidence. Carries the same `verdict` object when the
field is degraded, and each conflict row carries `samples` — the values
that cast nulled. The name is a **query parameter** (a catalog
key may contain `/`) and is ASCII-lowercased before lookup, mirroring
ingest's fold; an unpinned name returns 404.

Observations are **paged**: service names are client-chosen and their
observation rows are never removed, so one field's history can grow
without bound (it costs no pin slot). `limit` defaults to 100 and is
clamped to 1000 regardless of what the caller asks for; when more rows
follow, the response carries `services_cursor` — pass it back as `after`
for the next page. The cursor is opaque and keyset-based (a garbled one
returns 400, never a silent restart at page one). Conflict evidence needs
no cursor: it is capped per field at write time.

```
GET /api/v1/schema/conflicts
GET /api/v1/schema/conflicts?field=duration&service=envoy&since_secs=604800
```

The schema-health dashboard: recent type conflicts (a batch column whose
conforming cast nulled rows), most recent first, each with a bounded
`samples` set of the values it nulled. Filter by `field`, `service`, and
`since_secs`; `limit` defaults to 100 (max 1000).

```
POST   /api/v1/schema/field/ack?name=duration
DELETE /api/v1/schema/field/ack?name=duration
```

Acknowledge a degraded verdict, or withdraw the acknowledgement.
`schema_write`-gated: it changes what every read surface says about the
field. The name is a query parameter and is ASCII-folded before lookup,
like the other field routes.

```json
{ "note": "sender ships a fix on Friday" }
```

The note is optional, capped at 1024 bytes, stored verbatim and never
logged. A successful POST returns 200 with the acknowledgement:

```json
{ "acked_at": "2026-09-02T09:00:00.000000Z", "acked_by": "tkl_abc123",
  "note": "sender ships a fix on Friday", "evidence_through": 7 }
```

`evidence_through` is the point of the whole route. It is the count of
conflict episodes the ack covers, not a timestamp: compaction can record
several episodes inside one clock tick, so an ack keyed on time would
suppress evidence nobody had seen. The badge stays down while the field's
episode count is at or below that high-water and comes back the moment the
pin shelves another batch. Re-acknowledging advances the high-water and
replaces the note and the actor, but only when the incoming high-water is
at least the stored one: two operators acking a moment apart both get 200,
and the row keeps the name, note and timestamp of the one who covered more
evidence. `acked_by` is the key's stable prefix, not its display name,
because this row outlives renames and rotations.

A field whose evidence does not meet the degraded threshold answers **409**:
there is no verdict to acknowledge, and writing a high-water there would
swallow the evidence that first raises the badge. An unpinned name is a
**404**. DELETE answers **204** whether or not a row was there (not
acknowledged is the state the caller asked for either way) and 404 only for
an unpinned name.

The field detail carries a standing ack as `ack`, beside `verdict` rather
than instead of it: an acknowledgement overtaken by newer evidence appears
next to a re-raised verdict, and that pair is the story. A successful repin
of the field clears the ack outright, since the evidence it acknowledged no
longer describes the pin. That clear commits with the pin flip and is
logged as `field_degraded_ack_cleared` once per clear the server observes:
a crash between the commit and the log line loses the line, and the deleted
row leaves nothing to replay it from. The acknowledgement is gone either
way; only the audit trail is a line short.

```
POST /api/v1/schema/repin
```

Repin a field to a new catalog type (ADR-0011 slice B): a
shadow-generation rewrite of every affected file, with resurrection of
conflict-shelved values from `_raw`, an atomic crash-recoverable cutover,
and one job at a time install-wide. `schema_write`-gated; a query-only
node (ingest disabled) answers 503 — it does not own the data root.

```json
{ "field": "status", "to": "VARCHAR", "dry_run": true, "force": true,
  "max_nulled_rows": 250, "max_ambiguous_rows": 0 }
```

`to` is a catalog spelling: `BIGINT`, `DOUBLE`, `TIMESTAMP`, `BOOLEAN`,
`VARCHAR` or `SEVERITY` (case-insensitive). `SEVERITY` puts a sender's own
field on the OTel ladder, and takes an optional `dialect` — `"otel"`
(default) or `"syslog"` — which reads NUMERALS only; a `dialect` with any
other target is a 400 rather than an ignored field.

`force` alone used to be a blank check. It is now a number. A forced
request may state `max_nulled_rows` (rows the rewrite may null) and
`max_ambiguous_rows` (dialect-ambiguous numerals it may carry); either one
without `force` is a 400, since an unforced repin accepts no loss at all.
An unstated ceiling is derived from that job's own scan, `scan + max(scan /
10 rounded up, 10)`: ten percent headroom for proportional growth on a big
corpus, a flat floor of ten rows for a small one. The headroom exists
because the plan is a photograph of a moving corpus, and refusing on a
one-row drift would make `force` useless on a live install.

Both pairs come back on the job row: `max_nulled_rows` /
`max_ambiguous_rows` echo what the request asked for, and
`accepted_max_nulled_rows` / `accepted_max_ambiguous_rows` are what the job
is held to, resolved once at plan time. All four are absent rather than
zero when there is no number: an unstated request ceiling, an unforced job,
a job that has not scanned yet, and a job row written before ceilings
existed all read as "no number here". A finished rewrite worse than its
accepted ceilings refuses the cutover exactly as an unforced lossy plan
does, with the accepted and actual counts named in the reason.

The HTTP status carries the verdict, and the body is the job row in every
case:

- **200** — a dry-run report: affected files, rows carrying a value,
  `projected_nulls` (stored values the new type cannot read),
  `resurrectable` (shelved values `_raw` gives back), affected bytes,
  `ambiguous_numerals` (rows whose numeral reads as a different severity
  in each dialect), up to five `unmapped_samples` of the values the new
  type cannot read, `liveness` when something is still writing the field,
  and `requires_force` — whether the identical *executing* request would be
  refused. Dry runs are persisted jobs too — the row *is* the report, and
  `requires_force` is what keeps a 200 from reading as a green light.
- **202** — the rewrite started in the background; poll the status route.
- **400** — refused on the merits, side-effect-free: an unpinned field, a
  declared envelope field, an unknown target type, or `to` equal to the
  current pin without `force` (with `force` that shape runs a
  **resurrection-only** pass — re-extract shelved values under the same
  pin).
- **409** with a job body — the scan found something the request did not
  accept and no `force` flag was passed: values the new type cannot read,
  or (for a `SEVERITY` target read as `otel`) numerals 1-7, which the OTel
  ladder and syslog PRI read as different severities. The job is terminal
  `refused_needs_force`, the body is the plan the refusal is based on, and
  `requires_force_reason` names which of the two it was. Asserting
  `dialect: "syslog"` answers the ambiguity; `force` accepts either, up to
  the ceilings it binds, and a forced job over one of those ceilings is
  refused with the same status and a reason naming the accepted and the
  actual count. (A second repin while one runs also 409s, with the ordinary
  error envelope.)

The request holds open for the whole scan, which is a full-corpus pass —
minutes on a large archive, past most client and proxy timeouts. A
disconnect does not cancel anything: the claimed job runs to a terminal
status on its own and the verdict is readable from the status route, so a
timed-out repin is polled, never retried blind.

The same gate is asked again of the finished rewrite: ingest keeps running
for the whole job, so a file written after the scan can carry values the
new type cannot read. A job that started with 202 therefore still ends
`refused_needs_force` — corpus untouched, `rows_nulled` carrying what the
rewrite would have lost — when that happens without `force`, or with
`force` when the finished rewrite came in over the ceilings that job
accepted.

A forced lossy repin records its losses as `field_conflicts` evidence and
in `trawl_catalog_repin_rows_nulled_total`; the originals stay findable
in `_raw`. That evidence is written in the same transaction that flips the
pin and completes the job, so the status reading `succeeded` is a barrier:
the first read after it already sees the conflict rows, and a client that
polls the status route never has to poll `/schema/conflicts` behind it. Queries never observe a mixed-type corpus (the cutover holds
every query slot for its final seconds), events ingested during the
rewrite land exactly once, and a crash at any point is finished by the
next boot's marker replay.

```
GET /api/v1/schema/repin/status
```

The running job if any, else the newest job of any status —
`schema_read`-gated (read-only surfaces show repin state without offering
the trigger) and served on query-only nodes too.

The job row also carries `cancel_requested_at` and `cancelled_by` when
someone asked the job to stop: a `running` row carrying them is a cancel in
flight, and a `failed` row carrying them is a process that died between the
request and any boundary observing it.

```text
POST /api/v1/schema/repin/cancel
```

Ask the running repin to stop. `schema_write`-gated, no request body, and
a query-only node answers 503 like the trigger route. The HTTP status
carries the verdict and the body repeats it as `outcome`, with `detail` in
words and the job row under `job` when there is one:

- **202** `cancelling`: the request is accepted. The job stops at the next
  file boundary of its scan or build loop, sweeps any staging it had built,
  and ends `cancelled` with the live corpus untouched.
- **409** `past_point_of_no_return`: the job latched its cutover before the
  request arrived. The corpus is being swapped and there is nothing left to
  unwind, so the request is refused rather than queued, and the job
  completes normally.
- **404** `no_job_running`: no repin job is running on this node, or the one
  that was running has already chosen its outcome and there is nothing left
  to stop. The two read the same to a caller: nothing was cancelled, and the
  status route says how the job actually ended.

The latency contract is a boundary, not an instant. The scan and build
loops check before and after each file, but the whole-corpus snapshot walk
and the filesystem preflight are not checkpointed, so a job inside one of
those stops only when it leaves it. Early-phase cancels can therefore take
longer than one file.

A 202 accepts the request; it does not promise a terminal `cancelled`
status. The job's own completion can win the race, and a process that dies
between the request and any boundary acting on it lands `failed` with
`cancel_requested_at` and `cancelled_by` preserved (recovery never infers
`cancelled` from a request nothing acted on). Those two fields are written
durably as the request is accepted, but a process death in the same instant
as the request can still lose them, so treat the 202 as an accepted request
rather than a receipt for a durable one. Restarting trawld is the
stronger cancel: a killed job leaves the live corpus untouched and boot
recovery sweeps its staging. Read the outcome from the status route.

```text
POST /api/v1/schema/gc-pins
```

Reclaim pin slots held by fields nothing writes any more.
`schema_write`-gated, like the repin trigger; a query-only node answers
503, because proving a pin dead means reading parquet footers and it owns
none of them.

```json
{ "dry_run": true, "older_than_secs": 2592000 }
```

A pin is reclaimed only when **both** axes agree it is dead: no
`field_services` observation at or after the cutoff, **and** no standing
parquet under any live env directory declares the column. One axis alone
is not enough. Observations can lapse while a file still carries the
column, and a file can carry a column no live sender writes. The footer
scan runs under the compaction corpus gate, so nothing publishes between
the proof and the deletion.

`older_than_secs` defaults to 30 days and is accepted literally, `0`
included. The server then raises it to the retention window when that is
longer: a pin cannot be called dead over a span shorter than the corpus
trawl still keeps. The report names all three numbers
(`requested_older_than_secs`, `retention_floor_secs`,
`effective_older_than_secs`), and no client recomputes the window.

Three outcomes:

- **200**: the report, identical in shape for a dry run and a real one.
  `dry_run` and `deleted` are what tell them apart: a dry run mutates
  nothing at all (no delete, no cache eviction, no metric) and returns the
  candidates it would have reclaimed.
- **409**: refused, nothing mutated. Four shapes. A repin owns the data
  root (a `data/REPIN` marker, a staging or aside root, or a running job
  row: every footer under a corpus mid-rearrangement is provisional), or a
  repin claimed one mid-purge, which the purge transaction itself refuses.
  Another gc run is already in progress; a second one is turned away at
  once rather than queued behind a scan that holds the corpus gate. The
  corpus could not be read well enough to prove anything dead: a file that
  will not open, a `.parquet` that is not a regular file, an unparseable
  footer, a symlink under an env directory, a file that vanished mid-scan,
  or a data root that cannot be listed at all (an unmounted volume is
  UNKNOWN, never an empty corpus). Each is a file whose columns are
  unknown, and the whole run fails closed rather than deleting on partial
  evidence; the message names the count and up to three paths. Or the
  catalog store stopped answering one of gc's reads while the corpus gate
  was held, which is bounded at five seconds and reported as UNKNOWN.
- **503**: a query-only node, or the store is down. The purge is bounded
  in two phases and they answer differently. Everything before the commit
  is bounded twice — postgres' own five-second statement bound inside the
  transaction, and a ten-second client bound for the case postgres cannot
  see, a connection that stops answering while the backend sits idle.
  Cancelling there can only roll back, so nothing was reclaimed; gc
  re-reads the catalog for its candidates and drops from the pin cache
  every one postgres no longer holds, before it releases the corpus gate.
  The commit itself cannot be cancelled at all: postgres stops honouring
  cancellation once a commit is durable, so a commit that has not
  confirmed within thirty seconds is DETACHED rather than abandoned, and a
  commit that comes back with an error may have been applied by a backend
  that already made it durable. Both are UNKNOWN, and both drop every
  candidate from the pin cache without re-reading (a read would race the
  commit). Re-run with `dry_run` to see which way it went.

The deletion is metadata only: catalog rows and the in-process pin cache,
in one transaction, `repin_jobs` history untouched. Being wrong is cheap.
A reclaimed field that a sender writes again simply pins again from
scratch. Envelope and sender-asserted contract fields (`_time`, `service`
and the rest) are never candidates. One staleness residual: the unscoped
`/api/v1/schema` column listing is TTL-cached, so a reclaimed field can
still appear there for up to `schema_cache_ttl_secs` after the purge —
the same window that endpoint already carries for newly pinned fields. A
`?service=` request is served fresh.

```
GET /api/v1/schema/services
```

Rich per-service schema (per-column null counts, min/max, sizes, daily
volumes) from the background footer scan. Types come from the catalog's
in-process pin cache; a physically-present column with no pin (foreign or
boot-skipped parquet) reports the sentinel type `UNPINNED`.

Each service also carries `degraded_fields` — the degraded pins **this
service has actually conflicted on**, from the durable per-`(field,
service)` conflict aggregates. It is not a client-computable join:
carrying a degraded field's column is not evidence that this service is
what degraded it, so a service that only ever sent well-typed values for
`duration` is not listed under it. The key is **absent when empty** (the
ordinary case) and a server that predates it is indistinguishable from a
healthy install. Like the query notice, it is stamped from the
schema-refresh tick's in-process snapshot, so freshness is bounded by one
tick and no request-path query touches the aggregates.

```json
{ "services": [
    { "name": "nginx", "columns": [], "file_count": 12,
      "degraded_fields": ["duration"] }
  ] }
```

```
GET /api/v1/schema/values/{field}
```

Returns distinct values for a specific field. Useful for building autocomplete.

### Running queries

```
GET /api/v1/queries
```

List currently running queries.

```
DELETE /api/v1/queries/{id}
```

Cancel a running query by ID.

### Server info

```
GET /api/v1/stats
```

Server statistics (event counts, storage usage, uptime).

```
GET /api/v1/dashboard
```

Full dashboard snapshot. Admin only.

```
GET /api/v1/whoami
```

Returns the identity, kind, role names, and resolved trawl permissions for the current token.

Response shape:

```json
{
  "prefix": "abcd1234",
  "name": "siem-bot",
  "kind": "service",
  "roles": ["coastwatch-siem_consumer", "trawl-analyst"],
  "permissions": ["query", "schema_read", "validate", "saved_query", "export", "stream", "query_cancel"]
}
```

Fields:

- `kind` — `"human"` or `"service"`. Distinguishes interactive users from non-interactive principals.
- `roles` — the names of every data-defined role the key holds, sorted. Roles are cross-app permission bundles (ADR-0006), so the list is NOT app-scoped — it is display/audit metadata, never a gating input.
- `permissions` — the trawl-server-resolved permission union for the `trawl` namespace, in canonical order. Only permissions this server recognizes appear (unknown strings stored on a role are ignored, fail closed). Empty is impossible on the wire — a key resolving zero trawl permissions is rejected with 403 before reaching `/whoami`.

Per ADR-0006, roles are data: a single key can hold any number of roles, each role can span apps, and effective permissions are the union. Other consumers (e.g. coastwatch) resolve their own namespace from the shared keystore.

**Breaking change (ADR-0006 slice 1)**: the `assignments` array of `(app, role)` grants was replaced by `roles`; the `role_for` client helper is gone.

### Query history

```
GET /api/v1/history
```

Query execution history for the current token.

### Saved queries

```
GET    /api/v1/saved              # list saved queries
POST   /api/v1/saved              # create a saved query
PUT    /api/v1/saved/{id}         # update a saved query
DELETE /api/v1/saved/{id}         # delete a saved query
```

### Schedules

Attach cron-style schedules to saved queries for periodic execution.

```
PUT    /api/v1/saved/{id}/schedule    # create or update schedule
GET    /api/v1/saved/{id}/schedule    # get schedule
DELETE /api/v1/saved/{id}/schedule    # delete schedule
```

### Report runs

```
GET /api/v1/saved/{id}/runs           # list report runs (paginated)
GET /api/v1/saved/{id}/runs/{run_id}  # get a specific run with result data
```

### Export

```
POST /api/v1/export
Content-Type: application/json

{
  "query": "last=24h | stats count() by service",
  "format": "csv"
}
```

Export query results in `csv`, `json`, or `parquet` format.

### Streaming

```
GET /api/v1/stream?query=service%3Dnginx
```

Server-Sent Events (SSE) stream. Events are filtered in-memory against the event bus using a compiled matcher — no DuckDB involved. Supports back-pressure notification via `StreamEvent::Lagged`.

### Ingest

```
POST /api/v1/ingest
Content-Type: application/json

[{"timestamp": "...", "service": "myapp", "level": "info", "message": "..."}]
```

Accepts JSON arrays or ndjson. Supports optional gzip compression (`Content-Encoding: gzip`). Requires a token whose roles grant the `ingest` permission.

Every accepted event is canonicalized into the declared envelope (ADR-0009) — see the [Vector integration guide](/getting-started/vector-integration/) for the full field table, accepted wire aliases (`timestamp`/`@timestamp` → `_time`, `level` → severity derivation), and the severity token table. The policy is **repair when the server has an honest answer; reject when it would guess**:

- `_time` missing or unparseable → arrival time, repair code `time.from_ingest`; parseable but implausible (>10y past / >1d future) → kept, flagged `time.out_of_range`
- `env` missing → `default_env` (`env.defaulted`); present but not in the configured allowlist → **rejected** with a typed reason
- `host` missing → filled from the peer IP (`host.from_peer`) — **rejected** instead when the peer is in `trusted_relays`
- `service` missing, non-string, empty, over 128 bytes, containing invalid characters, or dot-leading → **rejected** with a typed reason
- unmappable severity → `severity` NULL, `severity.unmapped`, never a rejection
- client-sent `_ingested`/`_repairs` or a non-string `_raw` → stripped and replaced (`meta.stripped`)
- a field whose **name** carries ASCII uppercase → the name is folded to lowercase (`field.name_case_folded`). DuckDB identifiers are ASCII case-insensitive, so `Dur` and `dur` name the *same* column — folding at ingest keeps the catalog, parquet, and hot buffer on one spelling. When two keys in one event collide after folding (`Status` + `status`), one value is kept — the exact-lowercase spelling's when present, else the lexicographically first variant's — and the loser is dropped (`field.name_case_collision`). A case-variant of an envelope column with no exact counterpart is simply consumed as that column (`_Time` is the `_time` wire input); with the exact spelling present, the variant loses the collision, so the canonical value can never be shadowed. Original spellings stay findable in `_raw`.
- a field whose **name** exceeds 255 bytes → that field is dropped (`field.name_too_long`), the rest of the event is accepted. The field catalog keys on the name, and a name too long to be a postgres btree key could never be pinned — which would stall compaction for that service rather than lose one field. The name and its value stay findable in `_raw`.

Repairs are recorded per-event in `_repairs` (comma-separated codes, NULL when untouched) and counted in `trawl_ingest_repairs_total{code, service}`; they do not affect the `accepted`/`rejected` counts in the response. The `service` label is client-supplied, so it is capped at the first 256 distinct services seen since boot — repairs for services past the cap count under `service="<other>"` instead of growing the metric registry without bound. Rejections are per-event: valid siblings in the same batch still land.

Reserved field: `_trawl_wal_file` is trawl's own, used internally to carry each row's source WAL file through compaction. If an event supplies it, the key is silently dropped before the event is written — the rest of the event is accepted unchanged.

### Metrics

```
GET /metrics
```

Prometheus metrics. Unauthenticated. Outside the `/api/v1` namespace.
