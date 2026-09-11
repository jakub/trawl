---
title: HTTP API
description: REST API reference for trawld.
---

The trawl server exposes a REST API over HTTPS. All routes under `/api/v1` except `/health` require bearer token authentication, including `/ingest`.

## Authentication

Include your API token in the `Authorization` header:

```
Authorization: Bearer flt_your_token_here
```

API keys are managed with `fleet-admin` against the fleet postgres
keystore (`DATABASE_URL`). Replace `PREFIX` with the selected stable key prefix;
create roles before keys on a fresh database:

```bash
fleet-admin keys create --name "my-key" --kind human --role trawl-analyst
fleet-admin keys list
fleet-admin keys revoke PREFIX
```

### Roles and permissions

Roles are data-defined (ADR-0006): named, cross-app bundles of permission
strings stored in the fleet keystore, managed with `fleet-admin roles`.
A key holds any number of roles; effective permissions are the union.
The historical grant migration converts its tiers to the bundles below. A fresh
keystore starts with no roles; use [access administration](/operate/access/) to
create them. These are example bundles, not privileged role names:

| Role | Trawl permissions |
|------|------------|
| `trawl-admin` | `query`, `schema_read`, `validate`, `saved_query`, `export`, `stream`, `query_cancel`, `server_manage` |
| `trawl-analyst` | `query`, `schema_read`, `validate`, `saved_query`, `export`, `stream`, `query_cancel` |
| `trawl-reader` | `query`, `schema_read`, `query_cancel` |
| `trawl-ingest` | `ingest` |

Handlers gate on permissions, never role names — reshape the tiers with
`fleet-admin roles` without a deploy. A valid key that lacks the
permission a route asks for is refused **403** with error code `forbidden`.
A key resolving *no* recognized trawl permission at all is also refused 403
by the grant gate before the handler. Missing, malformed, invalid, expired,
and revoked credentials remain an opaque **401**.

`schema_write` is separate from these bundles and gates catalogue mutations.
It is registered but not granted by the migration. Repin has no human-key-kind
requirement. See [role assignment](/operate/access/#create-roles-and-keys) and
[catalog procedures](/operate/catalog/) before granting it.

## Endpoints

### Health

```
GET /api/v1/health
```

Unauthenticated. Returns server health status.

The response always contains exactly four component checks. Each value is
`ok` or `error`; failures do not include database diagnostics or filesystem
paths.

```json
{
  "status": "ok",
  "checks": {
    "duckdb": "ok",
    "auth_db": "ok",
    "storage_db": "ok",
    "data_path": "ok"
  },
  "version": "0.4.0"
}
```

An `auth_db`, `storage_db`, or `data_path` failure returns `degraded` with
HTTP 200. This status does not guarantee that authenticated requests can
run: an `auth_db` outage prevents new requests from authenticating and
returns HTTP 503 on protected routes. A `duckdb` failure returns
`unavailable` with HTTP 503. All healthy checks return `ok` with HTTP 200.

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

Read routes require `schema_read`. Mutation permissions and ingest-node requirements
are stated separately below. Use [catalog administration](/operate/catalog/) for procedures.

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
on it means `trawl schema repin`, which needs `schema_write` on an ingest-enabled node.
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

`evidence_through` counts conflict episodes, not timestamps. New episodes beyond
that count raise the badge again. Re-acknowledgement replaces actor, note, and time
only if its episode count is at least the stored count. Concurrent requests can
both return 200 while the larger count wins. `acked_by` is a stable key prefix.
See [acknowledgement lifecycle](/operate/catalog/#degraded-pins).

A field whose evidence does not meet the degraded threshold answers **409**:
there is no verdict to acknowledge, and writing a high-water there would
swallow the evidence that first raises the badge. An unpinned name is a
**404**. DELETE answers **204** whether or not a row was there (not
acknowledged is the state the caller asked for either way) and 404 only for
an unpinned name.

Field detail returns `ack` beside `verdict`; an old ack can remain beside a
newly raised verdict. Successful repin clears it in the pin-flip transaction.
`field_degraded_ack_cleared` is logged after that commit, so a crash can lose the
audit line without undoing the clear.

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

With `force`, `max_nulled_rows` and `max_ambiguous_rows` bound accepted loss and
ambiguous numerals. Either ceiling without force is 400. An omitted ceiling is
`scan + max(ceil(scan / 10), 10)`, resolved once from the job's own scan.
See [preview and force ceilings](/operate/catalog/#repin) for the CLI procedure.

Both pairs come back on the job row: `max_nulled_rows` /
`max_ambiguous_rows` echo what the request asked for, and
`accepted_max_nulled_rows` / `accepted_max_ambiguous_rows` are what the job
is held to, resolved once at plan time. All four are absent rather than
zero when there is no number: an unstated request ceiling, an unforced job,
a job that has not scanned yet, and a job row written before ceilings
existed all read as "no number here". A finished rewrite worse than its
accepted ceilings refuses the cutover exactly as an unforced lossy plan
does, with the accepted and actual counts named in the reason.

Successful starts, scan refusals, and dry runs return a `job` wrapper around
the job row. Validation and concurrent-job errors use the ordinary error envelope:

- **200**: a dry-run report, or a job cancelled while the request was scanning
  (inspect `job.status`). A dry-run report includes: affected files, rows carrying a value,
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

The request remains open during the full-corpus scan. Disconnect does not cancel
the job. After a timeout, correlate the running/newest status with the original
job before retrying; that route has no lookup by ID. See
[lost-response recovery](/operate/catalog/#resolve-a-lost-response).

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
the trigger) and served on query-only nodes too. It takes no job ID and returns `job: null`
when no job exists. A newer job can hide the outcome a caller was following.

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

Scan and build loops check cancellation before and after each file; the snapshot
walk and filesystem preflight are not checkpointed. A 202 is acceptance, not a
terminal `cancelled` guarantee. Completion can win the race. A process death can
leave `failed` with cancellation fields, or lose those fields before persistence.
After the cutover marker, recovery finishes forward. Do not treat a restart as
an unconditional cancellation. See [cancel and verify](/operate/catalog/#stopping-a-running-repin).

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

A candidate needs no observation at or after the cutoff and no standing Parquet
column under any live env. The footer proof and deletion hold the compaction
corpus gate. See [pin reclamation](/operate/catalog/#reclaiming-dead-pin-slots).

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
in one transaction, `repin_jobs` history untouched. A reclaimed field that a sender writes again simply pins again from
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
service has actually conflicted on**, from the durable per-`(field, service)` conflict aggregates. It is not a client-computable join:
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

Returns active, recent, and retained work. Retained entries include work ID,
kind, started state, and retained duration. Metadata visibility and cancellation
authority differ; see [capacity diagnosis](/operate/health/#inspect-capacity).

```
DELETE /api/v1/queries/{id}
```

Request cancellation by ID, including retained work. `server_manage` can cancel
any work; `query_cancel` can cancel work owned by the exact submitting key. An
acknowledgement does not prove the worker stopped. Binding already inside DuckDB
can retain a permit until it returns. Repeat cancellation is idempotent.

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

Attach a schedule to a saved query to run it periodically. Schedules are
fixed intervals, not cron expressions: a schedule has one period (`"1h"`,
`"24h"`) and a planned fire cursor, and there is no calendar syntax.

```
PUT    /api/v1/saved/{id}/schedule    # create or update schedule
GET    /api/v1/saved/{id}/schedule    # get schedule
DELETE /api/v1/saved/{id}/schedule    # delete schedule
POST   /api/v1/saved/{id}/run         # trigger one run now (query mode only)
```

Request body:

```json
{
  "interval": "1h",
  "window": "since_last",
  "lag": "5m",
  "max_runs": 100,
  "enabled": true
}
```

- `interval` is the period, a duration: a number and one of `s`, `m`, `h`,
  `d`, `w`. Minimum 60s, maximum 10 years.
- `window` says what each run covers (ADR-0018 ruling 6). Three spellings.
  `"since_last"` tiles: each run covers `[the previous run's window end, this fire - lag)`, so consecutive runs cover consecutive intervals and a
  failed run's gap is healed by the next success. A duration such as
  `"2h"` is a fixed trailing span, re-measured from every fire and never
  healing anything; it takes the same 60s floor as the interval. Absent is
  query mode, the legacy shape: the saved DSL runs verbatim and the run
  records no bounds.
- `lag` is the late-arrival allowance, default `"0s"`. It shifts *both* window
  bounds back, so it delays coverage rather than widening it: an event
  that landed after the boundary it belongs to is still inside the window
  that covers it. There is no 60s floor on `lag`, and it is meaningful
  only with a `window`.
- `enabled` defaults to `true`. It is honoured on create as well as on
  update, so `{"interval": "1h", "enabled": false}` on a saved query with
  no schedule yet creates a disabled one.

Response body:

```json
{
  "id": 3,
  "saved_query_id": 7,
  "interval": "1h",
  "interval_secs": 3600,
  "enabled": true,
  "window": "since_last",
  "lag": "5m",
  "lag_secs": 300,
  "covered_through": "2026-03-14T01:55:00.000000Z",
  "next_fire_at": "2026-03-14T03:00:00.000000Z",
  "total_runs": 2,
  "created_at": "2026-03-14T02:00:00Z",
  "updated_at": "2026-03-14T02:00:00Z"
}
```

- `window` is the normalized mode, `"since_last"` or a duration. Absent in
  query mode.
- `lag` and `lag_secs` are present exactly when `window` is. A windowed
  schedule with no lag reports `"0s"` and `0`, which is the value in
  force, not an absence.
- `covered_through` is the `since_last` watermark, the instant the next
  window starts from. It is not evidence that a run happened: a fresh tiling
  schedule is seeded at the origin of the coverage it owes,
  `next_fire_at - interval - lag`, before it has ever run, and only after a
  success does it become the end of the newest window a run covered. Use
  `total_runs` and `last_run` to ask whether anything has run. Absent for a
  fixed window and for query mode, neither of which claims coverage. A
  schedule that used to tile keeps its watermark stored across a mode
  change, so switching back to `since_last` resumes from it, but it is not
  reported while the schedule is in a mode that does not mean it.
- `next_fire_at` is the planned next fire instant. Always present, windowed
  or not. All three instants are RFC 3339, UTC, microseconds.

Report-run rows (in `last_run` here and in the run listings below) carry
four more fields:

- `window_start` and `window_end` are the half-open interval `[start, end)`
  the run covered.
- `window_truncated` reports completeness. `false` is the positive claim that the run covered
  everything it owed; `true` means the `since_last` catch-up gap exceeded
  `max_catchup_intervals` and the start was clamped forward.
- `window_kind` is `"since_last"` or `"fixed"`, the mode the run was
  *claimed* under, so an edit racing a run cannot change what the run
  means.

All four are absent together for a run that had no window: a query-mode
run, a manual run, or one from before the schedule grew a window. They are
never backfilled. Absent therefore means "not a windowed run", `false`
means "windowed and complete", and `true` means "windowed and clamped".

Four requests are refused with 400, each naming both sides of the
conflict, because the server has no basis for choosing which one to drop:

- a window on a saved query that carries its own time clause:

  ```
  schedule window "since_last" conflicts with the saved query's last= time clause; remove one side
  ```

  The check runs in both directions and under the saved query's row lock,
  so `PUT /api/v1/saved/{id}` editing a `last=` into the DSL of a query
  that already has a window is refused with the same message. The pair
  cannot be assembled by two racing requests.
- a window on a query that reads `from saved`, whose input is stored
  results rather than ingest events:

  ```
  schedule window "since_last" conflicts with the saved query's "from saved" source; stored report rows cannot receive a _time window
  ```
- a `lag` with no `window`:

  ```
  lag 300s needs a report window: without `window` the saved query owns its own time clause and trawl shifts no bounds. Set window to "since_last" or a duration, or drop lag
  ```
- a duration that does not parse, in any of the three fields. The message
  names the field and what that field would have taken, because a request
  carrying both a `window` and a `lag` would otherwise leave you guessing
  which one was rejected:

  ```
  invalid window: invalid interval format: "5x"; window takes "since_last" or a duration: a number and one of s, m, h, d, w
  ```

  A short window reports the interval floor it shares (`schedule interval 30s is below minimum of 60s`), and anything over ten years reports
  `duration 315360001s exceeds the maximum of 315360000 seconds (10 years)`.

A window over text that does not parse at all is refused too: `schedule window "2h" cannot be attached to a query that does not parse: ...`. Query
mode parses nothing, so a saved query with no window keeps storing whatever
text you give it.

`POST /api/v1/saved/{id}/run` runs a query-mode schedule immediately. On a
windowed one it is a 409:

```
schedule uses coverage mode "since_last"; manual runs are disabled for windowed schedules; watch GET /api/v1/saved/7/schedule (covered_through, next_fire_at)
```

The schedule owns what its reports cover, and a manual run would either
double-count a window or advance the watermark past coverage nothing
produced. It is a 409 rather than a 400 because the request becomes fine
again the moment the window is dropped.

### Report runs

```
GET /api/v1/saved/{id}/runs           # list report runs (paginated)
GET /api/v1/saved/{id}/runs/{run_id}  # get a specific run with result data
```

A run's `query` is the *resolved* text, not the saved text: for a windowed
run the scheduler prepends `earliest="<start>" latest="<end>" ` to the
saved DSL and stores the result, so a run reproduces by paste. The prefix
is a splice on the text rather than a re-render of the parse tree, which
keeps your comments and your spelling intact. That is also why the saved
query may not carry its own time clause.

Every successful run is recorded, including one that found no rows. A
zero-row run has no parquet file (there is no schema to write), so its
columns are stored as a compressed JSON blob; `GET /api/v1/saved/{id}/runs/{run_id}` returns those columns with an empty row
list,
and the run keeps its window like any other.

Reading runs back through the DSL follows from that. `| from saved <name> run=latest` resolves the newest successful run and refuses to look past
it, so a zero-row run answers as itself (as an empty typed source that
downstream stages bind against) instead of quietly serving an older
window's numbers. `run=N` resolves one run by id and answers identically.
`run=all` unions the runs that produced a *file*, so a zero-row run is not
a member and `_run_id` never names one. A run whose parquet write failed
and fell back to the blob holds rows with no file to point a query at, and
is a 409:

```
report run 42 produced 17 rows but no parquet result, so it cannot be read through `from saved`; fetch it at /api/v1/saved/7/runs/42 instead, or wait for the next scheduled run
```

The mechanism behind the windows, with a worked example, is in
[scheduled reports](/architecture/data-flow/#scheduled-reports).

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

The [event reference](/reference/events/) defines the envelope, derivation,
reserved-name handling, repair codes, and per-event rejection rules. Source fields
such as `timestamp` and `level` remain ordinary fields. An unmappable severity
source leaves `_severity` absent and increments the unmapped counter; it is not
a repair and does not erase the source value.

Repairs do not change accepted/rejected counts. Valid siblings in a batch still
land when another event is rejected. Repairs are stored in `_repairs` and counted
by `trawl_ingest_repairs_total{code,service}`. The service label is capped at the
first 256 distinct services since boot, with later services grouped as `<other>`.
See [connect and verify a sender](/operate/ingestion/) for a bounded ingest check.

### Metrics

```
GET /metrics
```

Prometheus metrics. Unauthenticated. Outside the `/api/v1` namespace.
