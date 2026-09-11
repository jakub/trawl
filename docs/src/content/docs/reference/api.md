---
title: HTTP API
description: Look up every trawld route with its permission, request and response shape, and error codes.
---

trawld serves a JSON API over HTTPS under the base path `/api/v1`. Request bodies are JSON and carry `Content-Type: application/json`. Responses are JSON unless a route says otherwise. The Prometheus scrape route, `/metrics`, sits outside the base path.

The examples on this page follow the same convention as [health checks](/operate/health/): `TRAWL_URL` names the HTTPS API and `TRAWL_CURL_CONFIG` names an owner-only curl config file that holds the `Authorization` header.

## Authentication

Every route under `/api/v1` except `/api/v1/health` requires a bearer token:

```
Authorization: Bearer flt_...
```

Keys and the roles that carry permissions are created with `fleet-admin`. See [access administration](/operate/access/).

| Status | Code | When |
|--------|------|------|
| 401 | `auth_error` | The header is missing or malformed, or the key is invalid, expired, or revoked. The response does not say which. |
| 403 | `forbidden` | The key is valid but holds no permission that trawld recognizes. The message is `no trawl grant for this key`. |
| 403 | `forbidden` | The key is valid but lacks the permission the route checks. The message is `insufficient permissions`. |
| 503 | `service_unavailable` | The keystore did not answer. The message is `auth backend unavailable`. |

### Roles and permissions

A role is a named bundle of permission strings in the Fleet keystore. A key can hold any number of roles, and its permissions are the union of those roles. trawld checks permissions, never role names. A permission string it does not recognize grants nothing. These are the permissions in the `trawl` namespace:

| Permission | Allows |
|------------|--------|
| `query` | Run queries, list running queries, and read or clear the key's query history |
| `schema_read` | Read the schema, the field catalog, conflict samples, and repin status |
| `validate` | Validate DSL without running it |
| `saved_query` | Create, read, update, and delete saved queries, schedules, and report runs |
| `export` | Export query results |
| `stream` | Subscribe to live event streams |
| `query_cancel` | Cancel the key's own running queries |
| `server_manage` | Read server stats and the dashboard, cancel any query, and see the text of system-owned retained work |
| `ingest` | Submit events to `/api/v1/ingest` |
| `schema_write` | Repin a field, reclaim pins, and acknowledge degraded pins |

`schema_read` exposes samples of event values in conflict evidence: up to 5 values per conflict row, each cut to 256 bytes.

## Conventions

### Error envelope

Every error that trawld itself produces has this body:

```json
{
  "error": {
    "code": "parse_error",
    "message": "expected a pipe stage",
    "details": [
      { "message": "expected a pipe stage", "span": { "start": 12, "end": 15 }, "label": "pipeline", "hint": "did you mean 'stats'?" }
    ]
  }
}
```

| Field | Type | Description |
|-------|------|-------------|
| `code` | string | One of the codes below |
| `message` | string | Human-readable summary |
| `details` | array | Present only for `parse_error`. Each entry has `message`, a `span` of byte offsets into the query text, and optional `label` and `hint`. |

| Code | Status | Meaning |
|------|--------|---------|
| `parse_error` | 400 | The DSL does not parse |
| `validation_error` | 400 | The DSL parses but fails semantic checks: unknown function, wrong arity, or a pipeline over the expression budget or stage cap |
| `result_too_large` | 400 | The result exceeded `max_result_rows` |
| `bad_request` | 400 | Malformed input. Also the code on every 409, which has no code of its own. |
| `ingest_error` | 400 | An ingest body that cannot be read, or a query page that runs past `max_result_rows` |
| `auth_error` | 401 | Authentication failed |
| `forbidden` | 403 | The key lacks a permission |
| `not_found` | 404 | No such resource, or the key does not own it. The response does not say which. |
| `rate_limited` | 429 | The key's per-minute bucket is empty |
| `too_many_streams` | 429 | `max_sse_connections` reached |
| `execution_error` | 500 | DuckDB failed or the query was cancelled. Details are logged, never returned. |
| `internal_error` | 500 | Anything else |
| `timeout` | 504 | The query started but did not finish within `timeout_secs` |
| `service_unavailable` | 503 | A dependency is down, the server is at capacity, or the node cannot serve the route |

Four refusals come from the HTTP framework before a handler runs, with an empty or plain-text body instead of the envelope: 413 when the body exceeds the size limit, 415 when a JSON route receives no `Content-Type: application/json`, 400 when the body is not valid JSON, and 422 when the JSON does not match the request shape.

Every response carries an `X-Request-Id` header holding a ULID. Quote it when reporting a problem.

### Rate limiting

trawld keeps one token bucket per API key and route class. The bucket refills continuously and holds one minute of requests, so a burst of up to the full per-minute quota passes before the first 429.

| Route class | Config key | Default | Applies to |
|-------------|------------|---------|------------|
| Interactive | `[server.rate_limit] default_rpm` | `100` | Every authenticated route except `/api/v1/ingest` |
| Ingest | `[server.rate_limit] ingest_rpm` | `1000` | `/api/v1/ingest`, for a key that holds `ingest` |

A key without `ingest` spends its interactive bucket when it posts to `/api/v1/ingest`. A `0` disables the class. When any of the key's roles sets `rate_rpm`, the largest such value replaces the class default in both classes. The 429 body is the error envelope with code `rate_limited`. No `Retry-After` or `X-RateLimit-*` headers are sent. `/api/v1/health` and `/metrics` are not rate limited.

### Pagination

Query results page with `limit` and `offset` in the request. The response echoes them in `pagination` with `returned`, the number of rows in this page, and sets `truncated` when the full result reached `max_result_rows`. History and run listings take `limit` and `offset` as query parameters and return `total`. Catalog listings take `limit` and return `truncated`. The field detail route pages its service rows with an opaque `services_cursor`.

### Timestamps

Every instant is RFC 3339 in UTC. Catalog, repin, pin reclamation, and schedule window instants carry microseconds and a `Z` suffix, for example `2026-08-01T10:00:00.000000Z`. History, saved query, schedule `created_at` and `updated_at`, and run `started_at` and `finished_at` carry a `+00:00` offset, for example `2026-03-14T02:00:00+00:00`.

### Body size limits

| Config key | Default | Applies to |
|------------|---------|------------|
| `[server] max_request_body_bytes` | `"128K"` | Every route except `/api/v1/ingest` |
| `[ingest] max_body_bytes` | `"16M"` | `/api/v1/ingest` |

Both accept sizes such as `"128K"` and `"1M"`. A body over the limit is refused with 413. A gzip ingest body may expand to at most 10 times its wire size.

### Query timeout

`[server] timeout_secs` (default `30`) is one absolute deadline per request, started after authentication. Queue waits, `from saved` resolution, and execution all spend from it. If the deadline passes before the query starts, the response is 503 `service_unavailable` with the message `server at capacity: the query was not started`. If it passes after the query starts, the response is 504 `timeout`. The deadline applies to `/api/v1/query`, `/api/v1/export`, and `/api/v1/schema/values/{field}`.

### CORS

trawld sends CORS headers only when `[server] cors_allowed_origins` is not empty. It then allows `GET` and `POST` with the `Authorization` and `Content-Type` headers from the listed origins.

## Health

### Server health

`GET /api/v1/health`

Permission: none. The route is unauthenticated.

Probes DuckDB, the Fleet keystore, the app-state store, and the data directory.

**Request**

```bash
curl --fail-with-body "$TRAWL_URL/api/v1/health"
```

**Response**

```json
{
  "status": "ok",
  "checks": { "duckdb": "ok", "auth_db": "ok", "storage_db": "ok", "data_path": "ok" },
  "version": "<installed-version>"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `status` | string | `ok` when every check passes. `degraded` when `auth_db`, `storage_db`, or `data_path` fails. `unavailable` when `duckdb` fails. |
| `checks` | object | Exactly four keys, each `ok` or `error`. No diagnostics or paths. |
| `version` | string | The trawld package version |

`degraded` does not prove that authenticated requests can run. An `auth_db` failure turns every authenticated route into a 503.

**Errors**

| Status | Code | When |
|--------|------|------|
| 503 | none | `status` is `unavailable`. The body is the health response, not the error envelope. |

## Query

### Run a query

`POST /api/v1/query`

Permission: `query`

Runs a DSL query and returns one page of rows. A query whose first stage is `from saved` reads a stored report run instead of the corpus.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `query` | body | string | yes | The DSL query |
| `limit` | body | integer | no | Rows per page. Default and cap: `[server] max_result_rows` (`100000`). |
| `offset` | body | integer | no | Rows to skip. Default `0`. `offset + limit` may not exceed `max_result_rows`. |
| `timezone` | body | string | no | Timestamp display zone: `"UTC"`, `"local"`, or a fixed offset such as `"+05:30"`. Default `"UTC"`. |

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" -H "Content-Type: application/json" \
  -d '{"query": "_severity>=error last=1h | stats count() by service"}' \
  "$TRAWL_URL/api/v1/query"
```

**Response**

```json
{
  "columns": [{ "name": "service" }, { "name": "count" }],
  "rows": [["nginx", 12], ["api", 3]],
  "truncated": false,
  "pagination": { "limit": 100000, "offset": 0, "returned": 2 },
  "degraded_fields": ["duration"],
  "severity_columns": ["sev"]
}
```

| Field | Type | Description |
|-------|------|-------------|
| `columns` | array | Column names in result order |
| `rows` | array | One array per row. Values are JSON null, booleans, numbers, strings, or arrays. |
| `truncated` | boolean | `true` when the full result reached `max_result_rows` |
| `pagination` | object | `limit`, `offset`, and `returned` for this page |
| `degraded_fields` | array | Fields the query bound whose pin is degraded, including fields filtered on and then projected away. Absent when empty. Stale by at most `schema_cache_ttl_secs`. |
| `severity_columns` | array | Result columns that hold OTel severity numbers other than `_severity`. Absent when empty. |

**Errors**

| Status | Code | When |
|--------|------|------|
| 400 | `parse_error` | The DSL does not parse |
| 400 | `validation_error` | Unknown function, wrong arity, or a pipeline over the expression budget or stage cap |
| 400 | `bad_request` | `timezone` is not `UTC`, `local`, or a fixed offset |
| 400 | `ingest_error` | `offset + limit` exceeds `max_result_rows` |
| 400 | `result_too_large` | The result exceeded `max_result_rows` |
| 404 | `not_found` | A `from saved` stage names a saved query the key does not own, or one with no successful run |
| 409 | `bad_request` | A `from saved` stage selects a run that has rows but no Parquet file. The message names the run route. |
| 500 | `execution_error` | DuckDB failed or the query was cancelled |
| 503 | `service_unavailable` | The deadline passed before the query started, or a cold file moved during the read. Retry. |
| 504 | `timeout` | The deadline passed after the query started |

## Validate

### Validate a query

`POST /api/v1/validate`

Permission: `validate`

Parses and checks a DSL query without running it. Field names are not checked against the schema.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `query` | body | string | yes | The DSL query |

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" -H "Content-Type: application/json" \
  -d '{"query": "_severity>=error | stats count() by host"}' \
  "$TRAWL_URL/api/v1/validate"
```

**Response**

```json
{ "valid": true, "errors": [], "formatted": "_severity>=error | stats count() by host" }
```

| Field | Type | Description |
|-------|------|-------------|
| `valid` | boolean | Whether the query parsed and passed semantic checks |
| `errors` | array | Error details in the envelope's `details` shape. Empty when valid. |
| `formatted` | string | The query in canonical formatting. Present only when valid. |

An invalid query is a 200 with `valid: false`. A pipeline over the expression budget reports as invalid here rather than as a 400.

**Errors**

| Status | Code | When |
|--------|------|------|
| none | | No errors beyond [Authentication](#authentication) and [Rate limiting](#rate-limiting) |

## Export

### Export query results

`POST /api/v1/export`

Permission: `export`

Runs a query and returns the rows as a file. Timestamps are UTC. The row cap is `[server] max_export_rows` (`1000000`) instead of `max_result_rows`.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `format` | query | string | no | `csv`, `json`, or `parquet`. Default `csv`. |
| `query` | body | string | yes | The DSL query |
| `limit` | body | integer | no | Row cap. Default and maximum: `max_export_rows`. |

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" -H "Content-Type: application/json" \
  -d '{"query": "last=24h | stats count() by service"}' \
  -o export.csv "$TRAWL_URL/api/v1/export?format=csv"
```

**Response**

The body is the file. The headers name its type:

| `format` | `Content-Type` | `Content-Disposition` |
|----------|----------------|-----------------------|
| `csv` | `text/csv; charset=utf-8` | `attachment; filename="export.csv"` |
| `json` | `application/x-ndjson` | `attachment; filename="export.ndjson"` |
| `parquet` | `application/vnd.apache.parquet` | `attachment; filename="export.parquet"` |

**Errors**

| Status | Code | When |
|--------|------|------|
| 400 | `parse_error` | The DSL does not parse |
| 400 | `validation_error` | Semantic check failed |
| 400 | none | `format` is not `csv`, `json`, or `parquet`. Plain-text body from the framework. |
| 500 | `execution_error` | DuckDB failed |
| 503 | `service_unavailable` | The deadline passed before the export started |
| 504 | `timeout` | The deadline passed after the export started |

## Stream

### Stream live events

`GET /api/v1/stream`

Permission: `stream`

Opens a Server-Sent Events stream of events that match the query as they arrive. trawld matches events in memory against the event bus, so the stream sees events before compaction. The pin snapshot is taken once, when the stream opens, and a repin reaches the stream only on reconnect.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `query` | query | string | yes | The DSL query, URL-encoded |

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" -N \
  "$TRAWL_URL/api/v1/stream?query=service%3Dnginx"
```

**Response**

```
event: data
data: {"_time":"...","service":"nginx","message":"GET /"}

event: lagged
data: {"missed": 40}
```

| Event | Data | When |
|-------|------|------|
| `data` | One row object | Each matching event, for a pipeline with no aggregation |
| `snapshot` | `{"columns": [...], "rows": [...]}` | For an aggregating pipeline, every 500 ms or every 100 matching events, whichever comes first, and only when new events arrived |
| `lagged` | `{"missed": n}` | The subscriber fell behind the event bus and `n` ingest batches were skipped. A batch holds one or more events, so the number of missed events is unknown. |

The stream closes when the pipeline finishes, for example after `| limit 10`.

**Errors**

| Status | Code | When |
|--------|------|------|
| 400 | `parse_error` | The DSL does not parse |
| 400 | `validation_error` | Semantic check failed |
| 400 | `bad_request` | The filter or pipeline cannot run live, or ingest is disabled on this node |
| 429 | `too_many_streams` | `[server] max_sse_connections` (`32`) streams are open |

## Ingest

### Ingest events

`POST /api/v1/ingest`

Permission: `ingest`

Accepts a batch of events and writes them to the WAL. The body is either a JSON array of event objects or newline-delimited JSON with one object per line. trawld detects the format from the first non-whitespace byte. The route exists only when `[ingest] enabled` is `true`. On a node with ingest disabled the server answers 404 with an empty body.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| body | body | array or ndjson | yes | Event objects. The [event reference](/reference/events/) defines the envelope, derivation, repairs, and per-event rejection rules. |
| `Content-Encoding` | header | string | no | `gzip` for a compressed body |

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" -H "Content-Type: application/json" \
  -d '[{"timestamp": "2026-09-11T08:00:00Z", "service": "myapp", "level": "info", "message": "started"}]' \
  "$TRAWL_URL/api/v1/ingest"
```

**Response**

```json
{ "accepted": 1, "rejected": 1, "errors": [{ "index": 1, "message": "missing message" }] }
```

| Field | Type | Description |
|-------|------|-------------|
| `accepted` | integer | Events written to the WAL |
| `rejected` | integer | Events refused by per-event validation. Omitted when `0`. |
| `errors` | array | One entry per rejected event: `index` is the zero-based array position or line number, counting blank lines. Omitted when empty. |

A rejected event does not stop its siblings. Repairs do not change `accepted` or `rejected`. A body whose first non-blank byte is not `[` is read as newline-delimited JSON, so a single event object is accepted and a line that is not an event object counts as one rejected event with the response still 200. See [connect and verify a sender](/operate/ingestion/) for an end-to-end check.

**Errors**

| Status | Code | When |
|--------|------|------|
| 400 | `ingest_error` | Empty body, invalid UTF-8, an unparseable or empty JSON array, a gzip body that fails to decode, or a gzip body that expands past 10 times its wire size |
| 413 | none | The body exceeds `[ingest] max_body_bytes` |
| 429 | `rate_limited` | The key's ingest bucket is empty |

## Schema

Read routes need `schema_read`. Routes that change the catalog need `schema_write`. Procedures live in [catalog administration](/operate/catalog/).

### Schema columns

`GET /api/v1/schema`

Permission: `schema_read`

Lists the pinned fields as columns, plus corpus facts from the data directory. Fields whose newest observation is older than the retention horizon are hidden. The horizon is the longest `max_age_days` across `[retention]` and every `[retention.env.<name>]`, and a `0` anywhere removes it. A field with no observation is always listed.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `service` | query | string | no | Only fields that service has carried. Served fresh from the catalog. |
| `all` | query | boolean | no | `true` lifts the retention horizon |

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/schema?service=nginx"
```

**Response**

```json
{
  "columns": [{ "name": "_time", "type": "TIMESTAMP" }, { "name": "service", "type": "VARCHAR" }],
  "file_count": 12,
  "cached": true,
  "earliest_date": "2026-08-01",
  "latest_date": "2026-09-11",
  "total_bytes": 5242880,
  "services": ["api", "nginx"],
  "hot_buffer_events": 120,
  "hot_buffer_bytes": 40960
}
```

| Field | Type | Description |
|-------|------|-------------|
| `columns` | array | `name` and `type` per field, in query result display order |
| `cached` | boolean | Whether the corpus facts came from the cache. The unscoped column list is cached under the same `[server] schema_cache_ttl_secs` (`60`). |
| `earliest_date`, `latest_date` | string | Oldest and newest date directory, `YYYY-MM-DD` |
| `hot_buffer_events`, `hot_buffer_bytes` | integer | Current hot buffer size. Absent when the node has no hot buffer. |

**Errors**

| Status | Code | When |
|--------|------|------|
| 503 | `service_unavailable` | The app-state store did not answer |

### Per-service schema

`GET /api/v1/schema/services`

Permission: `schema_read`

Returns per-service column statistics from the background Parquet footer scan.

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/schema/services"
```

**Response**

```json
{
  "services": [
    {
      "name": "nginx",
      "columns": [
        { "name": "duration", "type": "BIGINT", "null_count": 3, "total_count": 900, "min_value": "1", "max_value": "4200", "compressed_bytes": 2048 }
      ],
      "earliest_date": "2026-08-01",
      "latest_date": "2026-09-11",
      "file_count": 12,
      "total_bytes": 5242880,
      "total_events": 900,
      "daily_event_counts": [{ "date": "2026-09-11", "count": 120 }],
      "degraded_fields": ["duration"]
    }
  ],
  "cached": true
}
```

| Field | Type | Description |
|-------|------|-------------|
| `columns[].type` | string | The pinned type. `UNPINNED` for a column present in a file but absent from the catalog. |
| `columns[].min_value`, `columns[].max_value` | string | From Parquet column statistics. Absent when the file has none. |
| `daily_event_counts` | array | Per-day counts. Absent when empty. |
| `degraded_fields` | array | Degraded pins this service has itself conflicted on. Absent when empty. Stale by at most one schema-refresh tick. |
| `hot_buffer_events`, `hot_buffer_bytes` | integer | Absent when the node has no hot buffer |

**Errors**

| Status | Code | When |
|--------|------|------|
| 503 | `service_unavailable` | The background scan has not completed since boot |

### Field catalog

`GET /api/v1/schema/fields`

Permission: `schema_read`

Lists pinned fields with observation and conflict evidence.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `service` | query | string | no | Only fields that service has carried. Scopes `service_count`, `row_count`, `first_seen`, and `last_seen` to that service. |
| `since_secs` | query | integer | no | Only fields observed within the last N seconds |
| `limit` | query | integer | no | Default `500`, clamped to the pin capacity of `10000` |

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" \
  "$TRAWL_URL/api/v1/schema/fields?service=nginx&since_secs=604800"
```

**Response**

```json
{
  "fields": [
    {
      "name": "duration", "type": "BIGINT", "pinned_from": "nginx", "pinned_at": "2026-08-01T10:00:00.000000Z",
      "service_count": 2, "row_count": 900, "first_seen": "2026-08-01T10:00:00.000000Z", "last_seen": "2026-09-11T08:00:00.000000Z",
      "conflict_count": 12, "rows_nulled": 34,
      "verdict": { "since": "2026-08-01T10:00:00.000000Z", "services": 2, "episodes": 41, "rows_shelved": 1290, "samples": ["n/a", "pending"], "suggested_to": "VARCHAR" }
    }
  ],
  "pinned_total": 42,
  "pin_capacity": 10000,
  "truncated": false
}
```

| Field | Type | Description |
|-------|------|-------------|
| `pinned_from` | string | The service whose batch set the pin. `_declared` for an envelope field. |
| `first_seen`, `last_seen` | string | Absent when the field was never observed |
| `conflict_count` | integer | Conflict evidence rows retained for the field |
| `rows_nulled` | integer | Rows nulled across the retained evidence rows. Not a lifetime total. |
| `verdict` | object | Present only when the pin is degraded: the evidence spans at least 24 hours and shows either 100 rows shelved or 3 episodes. A verdict is install-wide even under `?service=`. |
| `verdict.rows_shelved` | integer | Lifetime rows the pin nulled |
| `verdict.samples` | array | A bounded sample of shelved values, newest first. Every value stays in `_raw`. |
| `verdict.suggested_to` | string | The catalog type the evidence suggests |
| `pinned_total`, `pin_capacity` | integer | Catalog fill, unfiltered |
| `truncated` | boolean | The listing was cut at `limit` |

**Errors**

| Status | Code | When |
|--------|------|------|
| 503 | `service_unavailable` | The app-state store did not answer |

### Field detail

`GET /api/v1/schema/field`

Permission: `schema_read`

Returns one field's pin, one page of its per-service observations, its retained conflict evidence, and its verdict and acknowledgement when they exist. The name is a query parameter, not a path segment. trawld lowercases it before lookup.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `name` | query | string | yes | The field name |
| `limit` | query | integer | no | Service rows per page. Default `100`, maximum `1000`. |
| `after` | query | string | no | The `services_cursor` from the previous page |

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" \
  "$TRAWL_URL/api/v1/schema/field?name=duration&limit=500"
```

**Response**

```json
{
  "name": "duration", "type": "BIGINT", "pinned_from": "nginx", "pinned_at": "2026-08-01T10:00:00.000000Z",
  "services": [{ "service": "nginx", "first_seen": "2026-08-01T10:00:00.000000Z", "last_seen": "2026-09-11T08:00:00.000000Z", "row_count": 900 }],
  "services_cursor": "2026-09-11T08:00:00.000000Z|nginx",
  "conflicts": [
    { "field": "duration", "service": "envoy", "observed_type": "VARCHAR", "expected_type": "BIGINT", "rows_nulled": 1, "samples": ["n/a"], "at": "2026-09-10T11:00:00.000000Z" }
  ],
  "verdict": { "since": "2026-08-01T10:00:00.000000Z", "services": 2, "episodes": 41, "rows_shelved": 1290, "samples": ["n/a", "pending"], "suggested_to": "VARCHAR" },
  "ack": { "acked_at": "2026-09-02T09:00:00.000000Z", "acked_by": "abcd1234", "note": "sender ships a fix on Friday", "evidence_through": 7 }
}
```

| Field | Type | Description |
|-------|------|-------------|
| `services` | array | One page, most recent first |
| `services_cursor` | string | Opaque. Pass it back as `after`. Absent on the last page. |
| `conflicts` | array | Most recent first, at most 100 per field. `samples` is empty for evidence a repin rewrite recorded. |
| `verdict` | object | As on the field catalog. Absent when the pin is not degraded. |
| `ack` | object | The standing acknowledgement. Independent of `verdict`: an old ack can sit beside a re-raised verdict. |

**Errors**

| Status | Code | When |
|--------|------|------|
| 400 | `bad_request` | `after` is not a cursor this server issued |
| 404 | `not_found` | The field is not pinned |
| 503 | `service_unavailable` | The app-state store did not answer |

### Conflicts

`GET /api/v1/schema/conflicts`

Permission: `schema_read`

Lists recent type conflicts across all fields, most recent first.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `field` | query | string | no | Only this field, lowercased before lookup |
| `service` | query | string | no | Only this service |
| `since_secs` | query | integer | no | Only conflicts recorded within the last N seconds |
| `limit` | query | integer | no | Default `100`, maximum `1000` |

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" \
  "$TRAWL_URL/api/v1/schema/conflicts?field=duration&since_secs=604800"
```

**Response**

```json
{
  "conflicts": [
    { "field": "duration", "service": "envoy", "observed_type": "VARCHAR", "expected_type": "BIGINT", "rows_nulled": 1, "samples": ["n/a"], "at": "2026-09-10T11:00:00.000000Z" }
  ],
  "truncated": false
}
```

**Errors**

| Status | Code | When |
|--------|------|------|
| 503 | `service_unavailable` | The app-state store did not answer |

### Field values

`GET /api/v1/schema/values/{field}`

Permission: `schema_read`

Samples distinct values of one field from Parquet, for autocomplete. Results are cached for `schema_cache_ttl_secs`. The sample runs through the query pool under the query deadline.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `field` | path | string | yes | The field name |
| `limit` | query | integer | no | Default `10`, maximum `100` |
| `service` | query | string | no | Sample only this service's files. Letters, digits, `_`, `-`, and `.` only. |

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" \
  "$TRAWL_URL/api/v1/schema/values/service?limit=20"
```

**Response**

```json
{ "field": "service", "values": ["api", "nginx"], "cached": false }
```

**Errors**

| Status | Code | When |
|--------|------|------|
| 400 | `bad_request` | `service` contains a character outside the allowed set |
| 500 | `execution_error` | DuckDB failed |
| 503 | `service_unavailable` | The deadline passed before the sample started |
| 504 | `timeout` | The deadline passed after the sample started |

### Acknowledge a degraded pin

`POST /api/v1/schema/field/ack`

Permission: `schema_write`

Records that an operator has seen a degraded verdict. The acknowledgement covers the conflict episodes that exist now, and the next episode raises the verdict again. A second acknowledgement replaces the actor, note, and time only when its episode count is at least the stored one, so two concurrent requests both return 200 and the larger count wins. A successful repin clears the acknowledgement. See [degraded pins](/operate/catalog/#acknowledge-a-degraded-pin).

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `name` | query | string | yes | The field name, lowercased before lookup |
| `note` | body | string | no | Stored verbatim and never logged. At most 1024 bytes. |

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" -H "Content-Type: application/json" \
  -d '{"note": "sender ships a fix on Friday"}' \
  "$TRAWL_URL/api/v1/schema/field/ack?name=duration"
```

**Response**

```json
{ "acked_at": "2026-09-02T09:00:00.000000Z", "acked_by": "abcd1234", "note": "sender ships a fix on Friday", "evidence_through": 7 }
```

| Field | Type | Description |
|-------|------|-------------|
| `acked_by` | string | The acknowledging key's stable 8-character prefix, not its name |
| `evidence_through` | integer | The conflict episode count the acknowledgement covers |

**Errors**

| Status | Code | When |
|--------|------|------|
| 400 | `bad_request` | `note` is over 1024 bytes |
| 404 | `not_found` | The field is not pinned |
| 409 | `bad_request` | The field is not degraded, so there is no verdict to acknowledge |

### Withdraw an acknowledgement

`DELETE /api/v1/schema/field/ack`

Permission: `schema_write`

Removes the acknowledgement. The verdict shows again if the evidence still qualifies.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `name` | query | string | yes | The field name, lowercased before lookup |

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" -X DELETE \
  "$TRAWL_URL/api/v1/schema/field/ack?name=duration"
```

**Response**

204 with no body, whether or not an acknowledgement existed.

**Errors**

| Status | Code | When |
|--------|------|------|
| 404 | `not_found` | The field is not pinned |

## Repin

### Repin a field

`POST /api/v1/schema/repin`

Permission: `schema_write`

Changes one field's pinned type across the stored corpus, resurrecting shelved values from `_raw` where the new type can read them. One job runs at a time, install-wide. The request stays open for the scan, and disconnecting does not cancel the job. Ingest keeps running during the rewrite, so a job that started with 202 can still end `refused_needs_force` when the finished rewrite exceeds what the request accepted. See [repin a field](/operate/catalog/#repin-a-field) and [recover a lost response](/operate/catalog/#recover-a-lost-response).

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `field` | body | string | yes | The field, lowercased before lookup |
| `to` | body | string | yes | `BIGINT`, `DOUBLE`, `TIMESTAMP`, `BOOLEAN`, `VARCHAR`, or `SEVERITY`. Case-insensitive. |
| `dialect` | body | string | no | For a `SEVERITY` target only: `otel` (default) or `syslog`, the ladder numerals 1 to 7 are read on |
| `dry_run` | body | boolean | no | Scan and report without rewriting. Default `false`. |
| `force` | body | boolean | no | Accept a lossy rewrite, or rerun resurrection under the same pin when `to` equals the current pin. Default `false`. |
| `max_nulled_rows` | body | integer | no | With `force`: the most rows the rewrite may null before the cutover is refused. Absent means the scan's projection plus 10% headroom, with a floor of 10 rows. |
| `max_ambiguous_rows` | body | integer | no | With `force`: the same bound for dialect-ambiguous numerals |

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" -H "Content-Type: application/json" \
  -d '{"field": "status", "to": "VARCHAR", "dry_run": true}' \
  "$TRAWL_URL/api/v1/schema/repin"
```

**Response**

```json
{
  "job": {
    "id": 7, "field": "status", "from_type": "BIGINT", "to_type": "VARCHAR", "dry_run": true, "force": false,
    "status": "succeeded", "requested_by": "ops-key", "started_at": "2026-09-11T08:00:00.000000Z", "finished_at": "2026-09-11T08:00:04.000000Z", "error": null,
    "files_total": 12, "rows_carrying": 900, "projected_nulls": 0, "resurrectable": 34, "affected_bytes": 5242880,
    "files_done": 0, "rows_rewritten": 0, "rows_nulled": 0, "rows_resurrected": 0,
    "ambiguous_numerals": 0, "requires_force": false
  }
}
```

| Field | Type | Description |
|-------|------|-------------|
| `status` | string | `running`, `succeeded`, `failed`, `refused_needs_force`, `blocked`, or `cancelled`. A dry run ends `succeeded`. |
| `files_total`, `rows_carrying`, `projected_nulls`, `resurrectable`, `affected_bytes` | integer | The scan plan: affected files, rows with a stored value, values the new type cannot read, shelved values `_raw` gives back, and bytes held twice during the rewrite |
| `files_done`, `rows_rewritten`, `rows_nulled`, `rows_resurrected` | integer | Rewrite progress and outcome |
| `dialect` | string | Present only for a `SEVERITY` target |
| `ambiguous_numerals` | integer | Rows whose numeral 1 to 7 reads as a different severity in each dialect |
| `unmapped_samples` | array | Up to 5 sample values the new type cannot read. Absent when empty. |
| `liveness` | object | `last_seen` and `service` when a sender is still writing the field. Absent otherwise. |
| `requires_force` | boolean | Whether the same request without `dry_run` would be refused. Absent until the scan has recorded its plan. |
| `requires_force_reason` | string | The refusal text. Present exactly when `requires_force` is `true`. |
| `cancel_requested_at`, `cancelled_by` | string | Present once someone asked the job to stop. On a `running` row a cancel is in flight. On a `failed` row the process died before a file boundary observed the request. |
| `max_nulled_rows`, `max_ambiguous_rows` | integer | The ceilings the request stated. Absent when it stated none. |
| `accepted_max_nulled_rows`, `accepted_max_ambiguous_rows` | integer | The ceilings the job is held to, resolved once at plan time. Absent on an unforced job and before the scan. |

**Errors**

| Status | Code | When |
|--------|------|------|
| 200 | none | A dry run, or a job cancelled while the request was still scanning. Read `job.status`. |
| 202 | none | The rewrite started. Poll [repin status](#repin-status). |
| 409 | none | The body is a job with status `refused_needs_force`: the scan found values the new type cannot read, or ambiguous numerals under the `otel` reading, and `force` was not set, or the finished rewrite exceeded an accepted ceiling. `requires_force_reason` names which. |
| 409 | `bad_request` | A repin job is already running |
| 400 | `bad_request` | `field` is an envelope field or not pinned, `to` is not a catalog type, `to` equals the current pin without `force`, `dialect` is unknown or given with a target other than `SEVERITY`, a ceiling is given without `force`, the pin changed between lookup and claim, the staging directory is on a different filesystem from the data root, or free disk is below the affected bytes plus the retention floor |
| 503 | `service_unavailable` | This node has ingest disabled and does not own the data root, or the job could not claim the corpus before its scan and ended `blocked` |

### Repin status

`GET /api/v1/schema/repin/status`

Permission: `schema_read`

Returns the running job, or else the newest job of any status. There is no lookup by job id, and a newer job hides an older one. Served on every node, including those with ingest disabled.

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/schema/repin/status"
```

**Response**

```json
{ "job": { "id": 7, "field": "status", "status": "running", "files_total": 12, "files_done": 4 } }
```

`job` is `null` when no repin has ever run. The job shape is the one [repin a field](#repin-a-field) returns.

**Errors**

| Status | Code | When |
|--------|------|------|
| 503 | `service_unavailable` | The app-state store did not answer |

### Cancel a running repin

`POST /api/v1/schema/repin/cancel`

Permission: `schema_write`

Asks the running repin to stop at its next file boundary. A 202 is acceptance, not a promise: a job that reaches its cutover first completes, and the status route reports how it ended. See [cancel the repin](/operate/catalog/#cancel-the-repin).

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" -X POST \
  "$TRAWL_URL/api/v1/schema/repin/cancel"
```

**Response**

```json
{ "outcome": "cancelling", "detail": "cancel accepted for the running repin job. ...", "job": { "id": 7, "status": "running", "cancel_requested_at": "2026-09-11T08:01:00.000000Z", "cancelled_by": "ops-key" } }
```

| Field | Type | Description |
|-------|------|-------------|
| `outcome` | string | `cancelling`, `past_point_of_no_return`, or `no_job_running`. Mirrors the status. |
| `detail` | string | The verdict in words |
| `job` | object | The job the verdict is about. Absent for `no_job_running` and when the job row could not be read. |

**Errors**

| Status | Code | When |
|--------|------|------|
| 202 | none | `outcome` is `cancelling`. The job sweeps its staging files and ends `cancelled` with the live corpus untouched. |
| 409 | none | `outcome` is `past_point_of_no_return`. The cutover has started and the job completes. |
| 404 | none | `outcome` is `no_job_running` |
| 503 | `service_unavailable` | This node has ingest disabled |

## Pin reclamation

### Reclaim dead pins

`POST /api/v1/schema/gc-pins`

Permission: `schema_write`

Deletes catalog pins for fields nothing writes any more. A candidate has no observation at or after the cutoff and no column in any live Parquet file. Envelope fields are never candidates. The deletion is metadata only, and a reclaimed field that a sender writes again pins again from scratch. See [reclaim unused pins](/operate/catalog/#reclaim-unused-pins).

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `dry_run` | body | boolean | no | Scan and report without deleting. Default `false`. |
| `older_than_secs` | body | integer | no | How long a field must have gone unobserved. Default 30 days. `0` is accepted. trawld raises the value to the retention horizon when that is longer. |

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" -H "Content-Type: application/json" \
  -d '{"dry_run": true, "older_than_secs": 2592000}' \
  "$TRAWL_URL/api/v1/schema/gc-pins"
```

**Response**

```json
{
  "dry_run": true,
  "decided_at": "2026-09-11T08:00:00.000000Z",
  "requested_older_than_secs": 2592000,
  "retention_floor_secs": 7776000,
  "effective_older_than_secs": 7776000,
  "pins_examined": 3,
  "files_scanned": 12,
  "candidates": [{ "field": "old_field", "type": "VARCHAR", "last_seen": "2026-05-01T00:00:00.000000Z", "services": 1 }],
  "deleted": 0
}
```

| Field | Type | Description |
|-------|------|-------------|
| `decided_at` | string | The instant the cutoff is measured from |
| `retention_floor_secs` | integer | The retention horizon. Absent when no finite horizon exists. |
| `effective_older_than_secs` | integer | The larger of the requested and floor values |
| `pins_examined` | integer | Pins with no recent observation, before the footer scan |
| `candidates` | array | Pins judged dead, sorted by field. `last_seen` is absent for a pin never observed. |
| `deleted` | integer | Rows deleted. Always `0` on a dry run. |

**Errors**

| Status | Code | When |
|--------|------|------|
| 409 | `bad_request` | A repin owns the data root or a repin job is running, another reclamation run is in progress, a Parquet file could not be read well enough to prove a pin dead, the catalog store stopped answering for 5 seconds while the corpus gate was held, or a repin claimed the catalog during the purge. Nothing was deleted. |
| 503 | `service_unavailable` | This node has ingest disabled, the app-state store is down, the purge gave up before commit after 60 seconds with nothing reclaimed, or the commit did not confirm within 30 seconds and its outcome is unknown. In the last case rerun with `dry_run` to see what the catalog holds. |

## Running queries

### Active, recent, and retained work

`GET /api/v1/queries`

Permission: `query`

Lists active, recently completed, and retained work from every key, with `own` marking the reader's. See [inspect capacity](/operate/health/#inspect-capacity).

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/queries"
```

**Response**

```json
{
  "active": [{ "id": 41, "user": "analyst", "role": "trawl-analyst", "query": "last=1h", "running_ms": 1200, "own": true }],
  "recent": [{ "id": 40, "user": "analyst", "query": "last=1h", "duration_ms": 80, "rows": 12, "error": null, "timed_out": false, "own": true }],
  "retained": [{ "id": 39, "kind": "query", "started": true, "retained_ms": 4000, "user": "analyst", "query": "last=7d | stats count()" }]
}
```

| Field | Type | Description |
|-------|------|-------------|
| `active[].role` | string | The key's role names, sorted and comma-joined. `none` when the key holds no roles. |
| `recent` | array | Most recent first. At most `[server] max_query_history` (`1000`) entries are kept. |
| `retained` | array | Work that still holds a pool permit after its request answered. Disjoint from `active`. |
| `retained[].kind` | string | `query`, `from_saved`, `export`, `scheduled`, `ping`, or `sample` |
| `retained[].started` | boolean | Whether the work passed its start transition |
| `retained[].user`, `retained[].query` | string | Present for key-submitted work. For system work, present only for a reader with `server_manage`. |

**Errors**

| Status | Code | When |
|--------|------|------|
| none | | No errors beyond [Authentication](#authentication) and [Rate limiting](#rate-limiting) |

### Cancel a running query

`DELETE /api/v1/queries/{id}`

Permission: `query_cancel` for the key's own work, `server_manage` for any work

Asks the pool to interrupt the work with this id, including retained work. Ownership is the submitting key's id, not its name. The response acknowledges the request and does not prove the worker stopped. Repeating the request is safe.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `id` | path | integer | yes | The id from `/api/v1/queries` |

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" -X DELETE "$TRAWL_URL/api/v1/queries/41"
```

**Response**

```json
{ "cancelled": true, "query_id": 41 }
```

`cancelled` is `false` when no work with that id is registered.

**Errors**

| Status | Code | When |
|--------|------|------|
| 403 | `forbidden` | The key holds neither permission, or holds `query_cancel` and did not submit this work. The message is `cannot cancel this query` in the second case. |

## History

### Query history

`GET /api/v1/history`

Permission: `query`

Lists the key's own completed queries, most recent first.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `limit` | query | integer | no | Default `100`, maximum `1000` |
| `offset` | query | integer | no | Default `0` |

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/history?limit=20"
```

**Response**

```json
{
  "entries": [{ "id": 12, "query": "last=1h", "executed_at": "2026-09-11T08:00:00+00:00", "duration_ms": 80, "row_count": 12, "status": "success" }],
  "total": 1
}
```

| Field | Type | Description |
|-------|------|-------------|
| `entries[].status` | string | `success`, `error`, or `timeout` |
| `total` | integer | All history rows for this key |

**Errors**

| Status | Code | When |
|--------|------|------|
| 503 | `service_unavailable` | The app-state store did not answer |

### Clear query history

`DELETE /api/v1/history`

Permission: `query`

Deletes the key's own history rows.

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" -X DELETE "$TRAWL_URL/api/v1/history"
```

**Response**

```json
{ "deleted": 12 }
```

**Errors**

| Status | Code | When |
|--------|------|------|
| 503 | `service_unavailable` | The app-state store did not answer |

## Saved queries

### Saved query list

`GET /api/v1/saved`

Permission: `saved_query`

Lists the key's saved queries, sorted by name, each with its schedule when one exists.

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/saved"
```

**Response**

```json
{
  "queries": [
    { "id": 7, "name": "errors-by-service", "query": "_severity>=error | stats count() by service", "created_at": "2026-09-01T09:00:00+00:00", "updated_at": "2026-09-01T09:00:00+00:00",
      "schedule": { "id": 3, "saved_query_id": 7, "interval": "1h", "interval_secs": 3600, "enabled": true, "total_runs": 2, "next_fire_at": "2026-09-11T09:00:00.000000Z" } }
  ]
}
```

`schedule` is absent when the saved query has none. Its shape is the one [set a schedule](#set-a-schedule) returns.

**Errors**

| Status | Code | When |
|--------|------|------|
| 503 | `service_unavailable` | The app-state store did not answer |

### Create a saved query

`POST /api/v1/saved`

Permission: `saved_query`

Stores a named query. The DSL is admitted before it is stored.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `name` | body | string | yes | Must match `[a-zA-Z0-9_-]+` and be unique for the key |
| `query` | body | string | yes | The DSL query |

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" -H "Content-Type: application/json" \
  -d '{"name": "errors-by-service", "query": "_severity>=error | stats count() by service"}' \
  "$TRAWL_URL/api/v1/saved"
```

**Response**

```json
{ "id": 7, "name": "errors-by-service", "query": "_severity>=error | stats count() by service", "created_at": "2026-09-01T09:00:00+00:00", "updated_at": "2026-09-01T09:00:00+00:00" }
```

**Errors**

| Status | Code | When |
|--------|------|------|
| 400 | `parse_error` | The DSL does not parse |
| 400 | `validation_error` | Semantic check failed |
| 400 | `bad_request` | `name` does not match `[a-zA-Z0-9_-]+` |
| 409 | `bad_request` | The key already has a saved query with that name |

### Update a saved query

`PUT /api/v1/saved/{id}`

Permission: `saved_query`

Replaces the query text and optionally the name. When the saved query has a windowed schedule, a new text that carries its own `last=`, `earliest=`, or `latest=` clause is refused.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `id` | path | integer | yes | The saved query id |
| `query` | body | string | yes | The new DSL query |
| `name` | body | string | no | The new name |

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" -X PUT -H "Content-Type: application/json" \
  -d '{"query": "_severity>=error | stats count() by host"}' \
  "$TRAWL_URL/api/v1/saved/7"
```

**Response**

The updated saved query, in the shape [create a saved query](#create-a-saved-query) returns.

**Errors**

| Status | Code | When |
|--------|------|------|
| 400 | `parse_error` | The DSL does not parse |
| 400 | `validation_error` | Semantic check failed |
| 400 | `bad_request` | Invalid name, or the new text conflicts with the schedule window. The window message names both sides. |
| 404 | `not_found` | No saved query with this id belongs to the key |
| 409 | `bad_request` | The new name is taken |

### Delete a saved query

`DELETE /api/v1/saved/{id}`

Permission: `saved_query`

Deletes the saved query, its schedule, its runs, and their result files.

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" -X DELETE "$TRAWL_URL/api/v1/saved/7"
```

**Response**

```json
{ "deleted": true }
```

**Errors**

| Status | Code | When |
|--------|------|------|
| 404 | `not_found` | No saved query with this id belongs to the key |

## Schedules

A schedule runs a saved query at a fixed interval. There is no calendar syntax. Durations are a number and one unit from `s`, `m`, `h`, `d`, `w`, with a ceiling of 10 years. The mechanism behind windows is in [scheduled reports](/architecture/reports-telemetry/#scheduled-reports).

### Set a schedule

`PUT /api/v1/saved/{id}/schedule`

Permission: `saved_query`

Creates or replaces the schedule on a saved query. On an update, `next_fire_at` moves to now only when `interval` or `window` changed. A change to `max_runs` or `enabled` keeps the planned fire. An existing `covered_through` is never reset by an edit.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `id` | path | integer | yes | The saved query id |
| `interval` | body | string | yes | The period. At least `60s`. |
| `window` | body | string | no | What each run covers. `"since_last"` tiles from the previous run's window end. A duration such as `"2h"` is a fixed trailing span, at least `60s`. Absent is query mode: the saved DSL runs verbatim. |
| `lag` | body | string | no | Late-arrival allowance. Shifts both window bounds back. Default `"0s"`. Requires `window`. |
| `max_runs` | body | integer | no | Stop after this many runs. Absent means unlimited. |
| `enabled` | body | boolean | no | Default `true`, on create and on update |

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" -X PUT -H "Content-Type: application/json" \
  -d '{"interval": "1h", "window": "since_last", "lag": "5m"}' \
  "$TRAWL_URL/api/v1/saved/7/schedule"
```

**Response**

```json
{
  "id": 3, "saved_query_id": 7, "interval": "1h", "interval_secs": 3600, "enabled": true,
  "window": "since_last", "lag": "5m", "lag_secs": 300,
  "covered_through": "2026-09-11T07:55:00.000000Z", "next_fire_at": "2026-09-11T09:00:00.000000Z",
  "total_runs": 2,
  "last_run": { "id": 42, "query": "earliest=\"2026-09-11T06:55:00.000000Z\" latest=\"2026-09-11T07:55:00.000000Z\" _severity>=error | stats count() by service", "status": "success", "started_at": "2026-09-11T08:00:00+00:00", "finished_at": "2026-09-11T08:00:01+00:00", "duration_ms": 900, "row_count": 4, "result_path": "scheduled/errors-by-service/run_42.parquet", "window_start": "2026-09-11T06:55:00.000000Z", "window_end": "2026-09-11T07:55:00.000000Z", "window_truncated": false, "window_kind": "since_last" },
  "created_at": "2026-09-01T09:00:00+00:00", "updated_at": "2026-09-11T08:00:00+00:00"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `interval` | string | Normalized: the largest unit that divides the period, for example `"90m"` for 5400 seconds |
| `max_runs` | integer | Absent when unlimited |
| `window` | string | `"since_last"` or a normalized duration. Absent in query mode. |
| `lag`, `lag_secs` | string, integer | Present exactly when `window` is. A windowed schedule with no lag reports `"0s"` and `0`. |
| `covered_through` | string | The `since_last` watermark, where the next window starts. Seeded at `next_fire_at - interval - lag` before the first run. Absent for a fixed window and in query mode. |
| `next_fire_at` | string | The planned next fire. Always present. |
| `last_run` | object | The newest run, in the [report run](#report-runs) shape. Absent before the first run. |

**Errors**

| Status | Code | When |
|--------|------|------|
| 400 | `bad_request` | `interval` does not parse: `invalid interval: invalid interval format: "5x"` |
| 400 | `bad_request` | `interval` is short: `invalid interval: schedule interval 30s is below minimum of 60s` |
| 400 | `bad_request` | `window` does not parse: `invalid window: invalid interval format: "5x"; window takes "since_last" or a duration: a number and one of s, m, h, d, w` |
| 400 | `bad_request` | `lag` does not parse: `invalid lag: invalid interval format: "5x"; lag takes a duration: a number and one of s, m, h, d, w` |
| 400 | `bad_request` | A duration is over 10 years: `duration 315360001s exceeds the maximum of 315360000 seconds (10 years)` |
| 400 | `bad_request` | `lag` without `window`: ``lag 300s needs a report window: without `window` the saved query owns its own time clause and trawl shifts no bounds. Set window to "since_last" or a duration, or drop lag`` |
| 400 | `bad_request` | The saved query carries a time clause: `schedule window "since_last" conflicts with the saved query's last= time clause; remove one side` |
| 400 | `bad_request` | The saved query reads stored rows: `schedule window "since_last" conflicts with the saved query's "from saved" source; stored report rows cannot receive a _time window` |
| 400 | `bad_request` | A window on text that does not parse: `schedule window "2h" cannot be attached to a query that does not parse: ...` |
| 404 | `not_found` | No saved query with this id belongs to the key |

### Schedule

`GET /api/v1/saved/{id}/schedule`

Permission: `saved_query`

Returns the schedule with its run statistics.

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/saved/7/schedule"
```

**Response**

The shape [set a schedule](#set-a-schedule) returns.

**Errors**

| Status | Code | When |
|--------|------|------|
| 404 | `not_found` | The saved query has no schedule, or does not belong to the key |

### Delete a schedule

`DELETE /api/v1/saved/{id}/schedule`

Permission: `saved_query`

Deletes the schedule, its runs, and their result files. The saved query stays.

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" -X DELETE "$TRAWL_URL/api/v1/saved/7/schedule"
```

**Response**

```json
{ "deleted": true }
```

**Errors**

| Status | Code | When |
|--------|------|------|
| 404 | `not_found` | The saved query has no schedule, or does not belong to the key |

### Run a saved query now

`POST /api/v1/saved/{id}/run`

Permission: `saved_query`

Starts one run outside the interval, on a schedule in query mode. The run counts against `max_runs`. The response returns at once with status `running`, and the run continues in the background.

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" -X POST "$TRAWL_URL/api/v1/saved/7/run"
```

**Response**

```json
{ "id": 43, "query": "_severity>=error | stats count() by service", "status": "running", "started_at": "2026-09-11T08:30:00+00:00" }
```

The shape is the [report run](#report-runs) summary with no window fields.

**Errors**

| Status | Code | When |
|--------|------|------|
| 400 | `bad_request` | The saved query has no schedule, `max_runs` is reached, or a run is already in progress |
| 404 | `not_found` | No saved query with this id belongs to the key |
| 409 | `bad_request` | The schedule has a window: `schedule uses coverage mode "since_last"; manual runs are disabled for windowed schedules; watch GET /api/v1/saved/7/schedule (covered_through, next_fire_at)` |

## Report runs

A run summary has these fields:

| Field | Type | Description |
|-------|------|-------------|
| `id` | integer | Run id |
| `query` | string | The text the run executed. For a windowed run, the scheduler prepends `earliest="<start>" latest="<end>" ` to the saved text. |
| `status` | string | `running`, `success`, `error`, or `timeout` |
| `started_at`, `finished_at` | string | `finished_at` is absent while running |
| `duration_ms`, `row_count` | integer | Absent while running |
| `error_message` | string | Present when `status` is `error` |
| `result_path` | string | Parquet result path relative to the data directory. Absent for a zero-row run, whose columns are stored as a compressed JSON blob. |
| `window_start`, `window_end` | string | The half-open interval `[start, end)` the run covered |
| `window_truncated` | boolean | `true` when a `since_last` catch-up gap exceeded `[scheduler] max_catchup_intervals` and the start was moved forward |
| `window_kind` | string | `since_last` or `fixed`, the mode the run was claimed under |

The four `window_*` fields are absent together for a run without a window: a query-mode run or a manual run.

### Runs of a saved query

`GET /api/v1/saved/{id}/runs`

Permission: `saved_query`

Lists runs of one saved query, most recent first.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `id` | path | integer | yes | The saved query id |
| `limit` | query | integer | no | Default `20` |
| `offset` | query | integer | no | Default `0` |

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/saved/7/runs?limit=10"
```

**Response**

```json
{ "runs": [{ "id": 42, "query": "...", "status": "success", "started_at": "2026-09-11T08:00:00+00:00", "finished_at": "2026-09-11T08:00:01+00:00", "duration_ms": 900, "row_count": 4, "result_path": "scheduled/errors-by-service/run_42.parquet" }], "total": 2 }
```

**Errors**

| Status | Code | When |
|--------|------|------|
| 503 | `service_unavailable` | The app-state store did not answer |

### Report run

`GET /api/v1/saved/{id}/runs/{run_id}`

Permission: `saved_query`

Returns one run with its result rows. trawld reads the Parquet file when `result_path` is set and the stored blob otherwise. A zero-row run returns its columns with an empty `rows`.

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/saved/7/runs/42"
```

**Response**

```json
{
  "id": 42, "query": "...", "status": "success", "started_at": "2026-09-11T08:00:00+00:00", "finished_at": "2026-09-11T08:00:01+00:00", "duration_ms": 900, "row_count": 4, "result_path": "scheduled/errors-by-service/run_42.parquet",
  "result": { "columns": [{ "name": "service" }, { "name": "count" }], "rows": [["nginx", 3]] }
}
```

`result` is absent for a run with status `running` or `error`.

**Errors**

| Status | Code | When |
|--------|------|------|
| 404 | `not_found` | The run does not exist, belongs to another key, or belongs to a different saved query |

### All runs

`GET /api/v1/runs`

Permission: `saved_query`

Lists the key's runs across every saved query, most recent first.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `limit` | query | integer | no | Default `20` |
| `offset` | query | integer | no | Default `0` |

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/runs"
```

**Response**

```json
{ "runs": [{ "net_id": 7, "net_name": "errors-by-service", "id": 42, "query": "...", "status": "success", "started_at": "2026-09-11T08:00:00+00:00" }], "total": 2 }
```

| Field | Type | Description |
|-------|------|-------------|
| `runs[].net_id`, `runs[].net_name` | integer, string | The owning saved query's id and name. The remaining fields are the run summary. |

**Errors**

| Status | Code | When |
|--------|------|------|
| 503 | `service_unavailable` | The app-state store did not answer |

### Run statistics

`GET /api/v1/runs/stats`

Permission: `saved_query`

Aggregates the key's runs.

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/runs/stats"
```

**Response**

```json
{ "total_runs": 12, "success_count": 11, "error_count": 1, "timeout_count": 0, "avg_duration_ms": 850 }
```

`avg_duration_ms` is absent when no run has completed.

**Errors**

| Status | Code | When |
|--------|------|------|
| 503 | `service_unavailable` | The app-state store did not answer |

## Server info

### Server stats

`GET /api/v1/stats`

Permission: `server_manage`

Returns pool and query counters.

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/stats"
```

**Response**

```json
{ "uptime_secs": 86400, "total_queries": 1200, "active_queries": 1, "pool_available": 3, "pool_capacity": 4, "pool_retained": 0 }
```

| Field | Type | Description |
|-------|------|-------------|
| `pool_available`, `pool_capacity` | integer | Free and total executor permits |
| `pool_retained` | integer | Permits held by work whose request already answered. A subset of `pool_capacity - pool_available`. |

**Errors**

| Status | Code | When |
|--------|------|------|
| none | | No errors beyond [Authentication](#authentication) and [Rate limiting](#rate-limiting) |

### Dashboard snapshot

`GET /api/v1/dashboard`

Permission: `server_manage`

Returns the latest snapshot the background collector produced.

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/dashboard"
```

**Response**

```json
{
  "hostname": "trawl.example.com", "listen_addr": "127.0.0.1:8080", "uptime_secs": 86400, "version": "<installed-version>", "healthy": true,
  "pool_capacity": 4, "pool_active": 1, "pool_retained": 0,
  "hot_buffer_events": 120, "hot_buffer_max_events": 100000, "hot_buffer_bytes": 40960, "hot_buffer_max_bytes": 268435456, "hot_buffer_batches": 3,
  "total_queries": 1200, "query_rate": 0.4, "query_errors": 2, "query_timeouts": 0,
  "ingest_events": 500000, "ingest_rate": 12.5, "ingest_rejected": 3,
  "syslog_enabled": true, "syslog_events_udp": 1000, "syslog_events_tcp": 200, "syslog_rate": 0.1, "syslog_parse_errors": 0, "syslog_dropped": 0, "syslog_tcp_connections": 2,
  "wal_files": 1, "wal_bytes": 8192,
  "last_compaction_secs": 4, "compaction_runs": 8640, "compaction_errors": 0,
  "parquet_files": 12, "parquet_bytes": 5242880,
  "sse_active": 0, "sse_max": 32,
  "scheduler_enabled": true, "scheduler_schedules": 1,
  "recent_queries": [], "active_queries": []
}
```

| Field | Type | Description |
|-------|------|-------------|
| `query_rate`, `ingest_rate`, `syslog_rate` | number | Smoothed per-second rates computed by the server |
| `query_errors`, `query_timeouts` | integer | Counts over the recent-query window |
| `last_compaction_secs` | integer | Seconds since the last successful compaction. `null` when none has run. |
| `recent_queries`, `active_queries` | array | The same entries as `/api/v1/queries` without `own` |

**Errors**

| Status | Code | When |
|--------|------|------|
| 503 | `service_unavailable` | The collector has not produced a snapshot since boot |

### Dashboard stream

`GET /api/v1/dashboard/stream`

Permission: `server_manage`

Pushes the dashboard snapshot over Server-Sent Events every 2 seconds. Before the first snapshot exists the stream stays open and sends nothing.

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" -N "$TRAWL_URL/api/v1/dashboard/stream"
```

**Response**

```
event: stats
data: {"hostname":"trawl.example.com","uptime_secs":86400, ...}
```

Each `stats` event carries the [dashboard snapshot](#dashboard-snapshot) as JSON.

**Errors**

| Status | Code | When |
|--------|------|------|
| 429 | `too_many_streams` | 8 dashboard streams are already open. This cap is separate from `max_sse_connections`. |

### Current key

`GET /api/v1/whoami`

Permission: any recognized trawl permission

Returns the identity behind the bearer token and the permissions this server resolved for it.

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/whoami"
```

**Response**

```json
{
  "prefix": "abcd1234",
  "name": "siem-bot",
  "kind": "service",
  "roles": ["another-app-role", "trawl-analyst"],
  "permissions": ["query", "schema_read", "validate", "saved_query", "export", "stream", "query_cancel"]
}
```

| Field | Type | Description |
|-------|------|-------------|
| `prefix` | string | The key's stable 8-character prefix. Use it as the actor id in audit logs. `name` can change, `prefix` cannot. |
| `kind` | string | `human` or `service` |
| `roles` | array | Every role on the key, sorted, including roles for other Fleet applications |
| `permissions` | array | The `trawl` permissions this server recognizes, in the order of the [permissions table](#roles-and-permissions). Never empty. A key with no recognized permission receives 403 before it reaches this route. |

**Errors**

| Status | Code | When |
|--------|------|------|
| none | | No errors beyond [Authentication](#authentication) and [Rate limiting](#rate-limiting) |

## Metrics

### Prometheus metrics

`GET /metrics`

Permission: none. The route is unauthenticated and outside `/api/v1`.

Renders the Prometheus text exposition format. Each scrape collects process metrics and trawld gauges first.

**Request**

```bash
curl --fail-with-body "$TRAWL_URL/metrics"
```

**Response**

```
# TYPE trawl_active_connections gauge
trawl_active_connections 1
# TYPE trawl_auth_failures_total counter
trawl_auth_failures_total{reason="unauthorized"} 3
```

The `Content-Type` is `text/plain; version=0.0.4; charset=utf-8`.

**Errors**

| Status | Code | When |
|--------|------|------|
| none | | The route always answers 200 |
