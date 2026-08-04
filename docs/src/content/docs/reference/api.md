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
`fleet-admin roles` without a deploy.

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
  "query": "level=error last=1h | stats count() by service"
}
```

Execute a DSL query. Returns results as a JSON array of row objects.

### Validate

```
POST /api/v1/validate
Content-Type: application/json

{
  "query": "level=error | stats count() by host"
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
walk; `cached` reports whether *they* were cached (columns are always
fresh).

Parameters:

| Parameter | Description |
|-----------|-------------|
| `service` | Only fields that service has carried. |
| `all` | `true` lifts the retention window: by default a field whose most recent observation predates `[retention] max_age_days` (default 90; 0 disables) is hidden. A field with no observations at all (e.g. the envelope on a fresh install) is always shown. |

Observations come from compaction, and — for a corpus that predates the
catalog — from the boot conformance pass, which backfills them from the
files it adopts (timestamps taken from each file's partition hour, not from
boot time). So `?service=` and the window answer for historical data too;
a corpus older than `max_age_days` that retention has not yet pruned needs
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

```
GET /api/v1/schema/field?name=duration
```

One field's detail: the pin, per-service observations, and retained
conflict evidence. The name is a **query parameter** (a catalog key may
contain `/`) and is ASCII-lowercased before lookup, mirroring ingest's
fold; an unpinned name returns 404.

```
GET /api/v1/schema/conflicts
GET /api/v1/schema/conflicts?field=duration&service=envoy&since_secs=604800
```

The schema-health dashboard: recent type conflicts (a batch column whose
conforming cast nulled rows), most recent first. Filter by `field`,
`service`, and `since_secs`; `limit` defaults to 100 (max 1000).

```
GET /api/v1/schema/services
```

Rich per-service schema (per-column null counts, min/max, sizes, daily
volumes) from the background footer scan. Types come from the catalog's
in-process pin cache; a physically-present column with no pin (foreign or
boot-skipped parquet) reports the sentinel type `UNPINNED`.

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
