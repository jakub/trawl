---
title: HTTP API
description: Look up every trawld route with its permission, request and response shape, and error codes.
---

trawld serves a JSON API over HTTPS under the base path `/api/v1`. Request bodies are JSON and carry `Content-Type: application/json`. Responses are JSON unless a route says otherwise. The Prometheus scrape route, `/metrics`, sits outside the base path.

The examples on this page follow the same convention as [health checks](/operate/health/): `TRAWL_URL` names the HTTPS API and `TRAWL_CURL_CONFIG` names an owner-only curl config file that holds the `Authorization` header. [Keep a key in a curl config file](/operate/access/#keep-a-key-in-a-curl-config-file) shows how to write it.

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
| `query` | Run queries, capture results and read the key's own [captured results](#captured-results), list running queries, and read or clear the key's query history |
| `schema_read` | Read the schema, the field catalog, conflict samples, and repin status |
| `validate` | Validate DSL without running it |
| `saved_query` | Create, read, update, and delete saved queries, schedules, and report runs |
| `export` | Export query results |
| `stream` | Subscribe to live event streams |
| `query_cancel` | Cancel the key's own running queries |
| `server_manage` | Read server stats and the dashboard, [preview ingest](#preview-ingest), cancel any query, and see the text of system-owned retained work |
| `ingest` | Submit events to `/api/v1/ingest` |
| `schema_write` | Repin a field, read a repin job the key requested by its id, reclaim pins, and acknowledge degraded pins |
| `investigation_read` | With `query`, list and read every investigation in the installation, with its steps, findings, and kept evidence |
| `investigation_write` | With `query` and `investigation_read`, create investigations, attach searches, keep rows, record findings and notes, and close or reopen any investigation |

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
| `details` | array | Present for `parse_error` and `validation_error`. Each entry has `message` and optional `span`, `label` and `hint`. A `span` holds byte offsets into the query text. A `parse_error` entry always has one. A `validation_error` entry has none, and a `validation_error` that trawld finds only while it runs the query, such as a `timechart` over a column that is not a timestamp, has no entries. |
| `conflict` | object | Present for `sequence_conflict` only: `expected`, the `expected_seq` the request carried, and `current`, the investigation's `seq` now. See [investigations](#investigations) |

| Code | Status | Meaning |
|------|--------|---------|
| `parse_error` | 400 | The DSL does not parse |
| `validation_error` | 400 | The DSL parses but fails semantic checks: unknown function, wrong arity, or a pipeline over the expression budget or stage cap |
| `result_too_large` | 400 | The result exceeded `max_result_rows`, or an aggregation after a Rust stage such as `extract kv` would drop input at a cap. The message names the cap and its limit. See [stream live events](#stream-live-events) for the caps. A capture also answers it when a whole result does not fit the capture's caps. See [shapes and caps](#shapes-and-caps) |
| `bad_request` | 400 | Malformed input. Also the code on a 409 that has no code of its own. |
| `capture_source_too_large` | 400 | A capture's window selects more stored data than `[capture] source_max_bytes`. Nothing was scanned. Narrow the window. See [admission](#admission-and-quotas) |
| `ingest_error` | 400, 413 | An ingest body that cannot be read. 413 when a gzip body decodes past `[ingest] max_body_bytes` |
| `ingest_batch_too_large` | 413 | An ingest request holds more events or bytes than external producers may place in the hot buffer, so it can never be admitted |
| `request_too_large` | 413 | A request body exceeds the router's body limit, `[server] max_request_body_bytes` or `[ingest] max_body_bytes`. The message names the setting and its value. See [body size limits](#body-size-limits). Through trawl-web, also the proxy's own refusal of a body over `[server] max_request_body_bytes`, over the proxy body budget of 128 MiB when that setting is above it, or, under `/api/auth/*`, over the login body limit of 16 KiB, before trawld is contacted. The message names the limit in the same words |
| `preview_too_large` | 413 | An ingest preview sample holds more than 500 events |
| `unsupported_encoding` | 415 | An ingest preview body carries a `Content-Encoding` other than `identity` |
| `auth_error` | 401 | Authentication failed |
| `forbidden` | 403 | The key lacks a permission |
| `not_found` | 404 | No such resource, or the key does not own it. The response does not say which. |
| `result_unavailable` | 409 | A captured result's record exists, but its file is missing. See [captured result outcomes](#captured-result-outcomes) |
| `sequence_conflict` | 409 | An overwrite of an investigation carried an `expected_seq` that is no longer its `seq`. The envelope's `conflict` member holds `expected` and `current`. See [investigations](#investigations) |
| `investigation_exists` | 409 | A create named an investigation id that exists with another creator or other fields |
| `investigation_closed` | 409 | The investigation is closed and takes nothing new until a writer reopens it |
| `evidence_budget_exceeded` | 409 | Keeping a result would take an investigation's or the installation's evidence over its budget. Not a 429: evidence frees only as it ages out or when an operator deletes an investigation. See [attach a capture](#attach-a-capture-to-an-investigation) |
| `result_gone` | 410 | A captured result expired, was pruned to fit a lowered quota, or its investigation's evidence retention ended or the investigation was deleted. The message says which. See [captured result outcomes](#captured-result-outcomes) |
| `rate_limited` | 429 | The key's per-minute bucket is empty. Sent with `Retry-After` |
| `too_many_streams` | 429 | `max_sse_connections` reached |
| `capture_busy` | 429 | The key's capture queue is full, the installation runs as many captures as it may, or the deadline passed during admission. For an [attached capture](#attach-a-capture-to-an-investigation) or a [keep or finding](#append-a-keep-a-finding-or-a-note) that keeps a result, that includes waiting for a database connection, a lock, or an answer from the database. Sent with `Retry-After`. See [admission](#admission-and-quotas) |
| `capture_quota_exceeded` | 429 | The key's or the installation's capture quota has no room for one more result. Sent with `Retry-After`. See [admission](#admission-and-quotas) |
| `execution_error` | 500 | DuckDB failed for a reason other than its resource budgets, or the query was cancelled. Details are logged, never returned. |
| `execution_error` | 400 | A result cell is an integer outside the signed and unsigned 64-bit range. The message names the column. See [Result columns](#result-columns). |
| `result_corrupt` | 500 | A captured result's file does not match its recorded digest. See [captured result outcomes](#captured-result-outcomes) |
| `internal_error` | 500 | Anything else |
| `timeout` | 504 | The query started but did not finish within `timeout_secs`. Through trawl-web, also the proxy's own 504 when trawld's response head did not arrive within `timeout_secs` plus 10 s; the message says trawld may still be running the work. Open streams and bodies in flight are never cut |
| `timeout` | 408 | trawl-web only: the request body did not arrive whole within 30 s. The request was not sent to trawld |
| `service_unavailable` | 503 | A dependency is down, the server is at capacity, or the node cannot serve the route |
| `hot_buffer_full` | 503 | The hot buffer has no room for an ingest request. Sent with `Retry-After` |
| `request_limit_reached` | 503 | trawld is at its `[server] max_concurrent_requests` limit, or a health or `/metrics` request is at the probe allowance (fixed at 3), or a query list or cancel request is at the control allowance (fixed at 4). The handler did not run and nothing was ingested. No `Retry-After`. Retry with backoff. See [the request limit](/reference/configuration/#the-request-limit). Through trawl-web, also the proxy's own refusal when it is at its request count (fixed at 64, `/healthz` excepted) or when its budget for buffered request bodies (fixed at 128 MiB) cannot cover the request's declared body; the message names trawl-web as the hop and the request was not sent to trawld |
| `corpus_recovering` | 503 | The server cannot yet count every stored event exactly once, so it refuses the read. No `Retry-After`. See [corpus recovering](#corpus-recovering) |
| `query_resource_exhausted` | 503 | The query needed more spill or memory than the query database may use. No `Retry-After`. See [query resource exhausted](#query-resource-exhausted) |

#### Corpus recovering

A read answers 503 `corpus_recovering` while the corpus is unsettled: after a restart, until compaction proves that the WAL from before the restart is covered, or while a daily rollup marker is unfinished. It applies to [queries](#run-a-query), [captures](#capture-a-result) of the corpus, [exports](#export-query-results), uncached [field values](#field-values), [manual runs](#run-a-saved-query-now), and [repins](#repin-a-field). The message is fixed for each reason and names no count, path, or marker:

| Reason | Message |
|--------|---------|
| `restart_backlog` | `Search is unavailable while the server finishes loading data from before its restart.` |
| `rollup_pending` | `Search is unavailable while the server finishes an interrupted storage rollup.` |

When both reasons hold, the response names `rollup_pending`. The reason is the `cause_kind` of the [`http_failure` event](/operate/health/#trace-a-server-failure), and `checks.corpus` in [server health](#server-health) names it too. The envelope carries no reason field. Live tail, `| from saved` queries, `/api/v1/schema`, `/api/v1/schema/services`, health, metrics, and a cached field-values hit answer as usual. See [reads while the corpus is unsettled](/architecture/data-flow/#reads-while-the-corpus-is-unsettled).

trawld checks the key and the route's permission before it reads a byte of the body. A missing or bad key gets 401 and a key without the permission gets 403, whatever the body holds. trawld sends that refusal without waiting for the body, and hangs up without reading the rest of it. A client that is still sending the body when the refusal comes can see the connection close or reset instead of the response. trawld reads a body only when its key may use the route, and reads it under the size limit: a body over the limit is refused with 413 and the envelope, code `request_too_large`. After the same checks, a JSON body that cannot be read gets 400 with the envelope, code `bad_request`. That covers a body sent without `Content-Type: application/json`, a body that is not valid JSON, and JSON that does not match the request shape. No route answers with the HTTP framework's own 415 or 422.

Every response carries an `X-Request-Id` header holding a ULID. Quote it when reporting a problem. The trawl CLI and trawl-client end the message of every error that got a response with `(request <id>)`.

#### Query resource exhausted

A query, export, or field sample answers 503 `query_resource_exhausted` when it needs more spill or memory than the query database may use: more than `[server] query_spill_max_bytes` of spill, or more than `[server] query_memory_limit` of memory. DuckDB reports both limits with one error type, so the response does not say which one ran out. The body is fixed:

```json
{
  "error": {
    "code": "query_resource_exhausted",
    "message": "query could not complete with the available query resources; narrow the time range or simplify the query"
  }
}
```

The message names no path, size, or DuckDB text. The response carries no `Retry-After`. The status is 503, not 500, because nothing is broken, and not a 4xx, because every query shares the budgets: the same query can finish on an idle server and fail beside a heavy one. Narrow the time range, add a filter, or drop a wide `stats ... by` before you retry. The server keeps serving, and a small query on the same server succeeds.

The `http_failure` event records `error_class=service_unavailable` and `cause_kind=duckdb_resource_exhausted`. A query that timed out first keeps its 504 `timeout`, even when DuckDB then fails with the same error. The rule covers planning, execution, row fetch, and export. See [query scratch and budgets](/reference/configuration/#query-scratch-and-budgets) for the settings and [diagnose a 503 or 504 from a query](/operate/health/#diagnose-a-503-or-504-from-a-query) for the operator steps.

### Rate limiting

trawld keeps one token bucket per API key and route class. The bucket refills continuously and holds one minute of requests, so a burst of up to the full per-minute quota passes before the first 429.

| Route class | Config key | Default | Applies to |
|-------------|------------|---------|------------|
| Interactive | `[server.rate_limit] default_rpm` | `100` | Every authenticated route except `/api/v1/ingest` |
| Ingest | `[server.rate_limit] ingest_rpm` | `1000` | `/api/v1/ingest`, for a key that holds `ingest` |

A key without `ingest` spends its interactive bucket when it posts to `/api/v1/ingest`. A `0` disables the class. When any of the key's roles sets `rate_rpm`, the largest such value replaces the class default in both classes. The 429 body is the error envelope with code `rate_limited`. It carries `Retry-After`: the seconds until the bucket holds a token again, rounded up, and never less than 1. No response carries `X-RateLimit-*` headers. Three other responses carry `Retry-After`: 503 `hot_buffer_full` from [ingest](#ingest-events), and 429 `capture_busy` and `capture_quota_exceeded` from [captures](#admission-and-quotas). `/api/v1/health` and `/metrics` are not rate limited.

### Pagination

Query results page with `limit` and `offset` in the request. The response echoes them in `pagination` with `returned`, the number of rows in this page, and `total`, the number of rows the execution produced before the page was cut from them. History and run listings take `limit` and `offset` as query parameters and return `total`. Catalog listings take `limit` and return `truncated`. The field detail route pages its service rows with an opaque `services_cursor`.

### Result columns

A result is `columns` and `rows`. The query response, a report run's `result`, and the stream's `data` and `snapshot` frames all carry one. Each column is an object with a `name` and a required `type`:

```json
{ "name": "codes", "type": "list", "items": { "type": "integer" } }
```

`type` comes from a closed set: `boolean`, `integer`, `float`, `string`, `timestamp`, `date`, `time`, `list`, and `mixed`. A `list` column also has `items`, the type of its elements, in the same object shape. A client must refuse a column whose `type` it does not know. The type comes from execution, not from the catalog, so a zero-row result is typed too.

Each cell is encoded under its column's type. A null cell is JSON `null` under every type.

| `type` | Cell |
|--------|------|
| `boolean` | `true` or `false` |
| `integer` | A decimal string, for example `"9007199254740993"` |
| `float` | A JSON number, or one of the strings `"NaN"`, `"Infinity"` and `"-Infinity"` |
| `string` | A string |
| `timestamp` | RFC 3339 with six fraction digits and an explicit offset, for example `"2026-10-22T12:34:56.123456Z"`, or `"infinity"` or `"-infinity"` |
| `date` | `YYYY-MM-DD`, for example `"2026-10-22"` |
| `time` | `HH:MM:SS`, with a fraction when it has one, for example `"12:34:56.123456"` |
| `list` | An array of cells, each encoded under `items` |
| `mixed` | An object that tags the cell with its own type, for example `{"type": "integer", "value": "200"}` |

An integer cell is a string so that a JavaScript client keeps every digit past 2^53. The spelling is canonical: `-?(0|[1-9][0-9]*)`. A decimal string has no plus sign, no leading zero, no `-0`, and no whitespace. The value is in the signed or the unsigned 64-bit range, from `-9223372036854775808` to `18446744073709551615`. A decoder refuses any other spelling and any value outside that range. It also refuses a JSON number under `integer`, and a string under `float` other than the three spellings in the table. A query whose result has an integer outside the range, for example a `sum` past `18446744073709551615`, is refused with 400 `execution_error`. The message names the column. Cast the column with `tonumber()` or `tostring()`.

A timestamp cell uses `Z` at offset zero and `+HH:MM` or `-HH:MM` otherwise. A query response writes timestamps in the request's `timezone`. Report runs and stream frames write them in UTC. A year outside 0000 to 9999 uses the ISO 8601 expanded form, a sign and at least four digits, for example `+10000-01-01T00:00:00.000000Z`. `infinity` and `-infinity` are DuckDB's infinite timestamps.

A `mixed` column holds cells of more than one type. Only a column that an `extract kv` stage computes, or a merge of live frames, can be `mixed`. Each non-null cell carries its own `type` and `value`, and a `list` tag also carries `items`. A decoder reads the cell from its tag, never from its spelling.

A column with no evidence of its type is `string`. That is an all-null column that a stage after `extract kv` computes, such as `let x = null`, or a field that is null in a live `data` frame. A column that a stage after `extract kv` passes on from its input, renamed or not, takes the input column's type when its own cells give no evidence, so a result with no rows left still has typed columns. When you merge live frames, treat an all-null `string` column as no evidence.

These limits apply:

- DuckDB `DECIMAL`, `INTERVAL`, `STRUCT`, `MAP`, and `UNION` values arrive as null cells in a `string` column.
- A timestamp that a stage after `extract kv` computes is written in UTC, not in the request's `timezone`.
- A `date` or `time` column that passes through a stage after `extract kv` is `string`.
- A `mixed` column tags a timestamp that a stage after `extract kv` computes as `string`, on its own or as a `list` item. A column whose every cell is such a timestamp is `timestamp`.

The schema routes keep DuckDB type names, such as `BIGINT` and `VARCHAR`. The lowercase `type` vocabulary is for result columns only.

### Timestamps

Every instant is RFC 3339 in UTC, except result cells, which [Result columns](#result-columns) describes. Catalog, repin, pin reclamation, schedule window, and captured result instants carry microseconds and a `Z` suffix, for example `2026-08-01T10:00:00.000000Z`. History, saved query, schedule `created_at` and `updated_at`, and run `started_at` and `finished_at` carry a `+00:00` offset, for example `2026-03-14T02:00:00+00:00`.

### Body size limits

| Config key | Default | Applies to |
|------------|---------|------------|
| `[server] max_request_body_bytes` | `"128K"` | Every route except `/api/v1/ingest` |
| `[ingest] max_body_bytes` | `"16M"` | `/api/v1/ingest` |

Both accept sizes such as `"128K"` and `"1M"`. These two keys are the only body bounds. A body over the limit is refused with 413 `request_too_large`, whether `Content-Length` announced the size or trawld found it while reading a chunked body. The message names the setting and its value, for example `request body exceeds [server] max_request_body_bytes (131072 bytes)`. trawld hangs up on an oversized body and does not read the rest of it. While trawld is at its [request limit](/reference/configuration/#the-request-limit), the 503 `request_limit_reached` comes before the 413. A client that is still uploading can lose the 413 to a connection reset. The trawl CLI and client then report `network error: the server closed the connection before the upload finished; the request may exceed the server's request size limit, or the server may be at its request limit`. A gzip ingest body is held to `[ingest] max_body_bytes` twice: once as sent, and once decoded. trawld stops decoding one byte past the limit and answers 413 `ingest_error`.

### Query timeout

`[server] timeout_secs` (default `30`) is one absolute deadline per request, started after authentication. Queue waits, `from saved` resolution, and execution all spend from it. If the deadline passes before the query starts, the response is 503 `service_unavailable` with the message `server at capacity: the query was not started`. If it passes after the query starts, the response is 504 `timeout`. The deadline applies to `/api/v1/query`, `/api/v1/export`, and `/api/v1/schema/values/{field}`. A capture has a shorter deadline of its own, `[capture] deadline_secs`, which `timeout_secs` caps. See [admission and quotas](#admission-and-quotas).

### CORS

trawld sends CORS headers only when `[server] cors_allowed_origins` is not empty. It then allows `GET` and `POST` with the `Authorization` and `Content-Type` headers from the listed origins.

## Health

### Server health

`GET /api/v1/health`

Permission: none. The route is unauthenticated.

Reports the query engine's latest sample, and probes the Fleet keystore, the app-state store, and the data directory. The `duckdb` check reads a sample that trawld takes on its own schedule, so a health request never waits for the engine and never takes an executor permit.

**Request**

```bash
curl --fail-with-body "$TRAWL_URL/api/v1/health"
```

**Response**

```json
{
  "status": "ok",
  "checks": { "duckdb": "ok", "auth_db": "ok", "storage_db": "ok", "data_path": "ok", "ingest_capacity": "ok", "corpus": "ok" },
  "version": "<installed-version>"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `status` | string | `ok` when every check passes. `degraded` when `duckdb` reads `slow`, `auth_db`, `storage_db`, or `data_path` reads `error`, `ingest_capacity` reads `refusing`, or `corpus` reads anything but `ok`. `unavailable` when `duckdb` reads `error`. |
| `checks` | object | Exactly six keys. `duckdb` is `ok`, `slow`, or `error`. `auth_db`, `storage_db`, and `data_path` are each `ok` or `error`. `ingest_capacity` is `ok` or `refusing`. `corpus` is `ok`, `rollup_pending`, or `restart_backlog`. No diagnostics or paths. |
| `version` | string | The trawld package version |

`degraded` does not prove that authenticated requests can run. An `auth_db` failure turns every authenticated route into a 503.

`duckdb` reads the latest engine sample. Every 10 seconds, a dedicated thread runs `SELECT 1` on a connection of its own to the query database, with a 250 ms budget. A sample that runs past the budget is interrupted, and no new sample starts until it returns.

| `duckdb` | Meaning | `status` | HTTP |
|----------|---------|----------|------|
| `ok` | The latest sample succeeded within its budget | `ok` unless another check differs | 200 |
| `slow` | The latest sample ran past its budget or failed with `Out of Memory`, and a sample succeeded in the last 60 seconds | `degraded` | 200 |
| `error` | The latest sample returned any other DuckDB error, no sample has succeeded in the last 60 seconds, or none has succeeded since boot | `unavailable` | 503 |

A busy engine does not turn the check to `error` while a sample has succeeded in the last 60 seconds. A full executor pool, permits held after a timeout, and live streams do not change `duckdb` or `status`, because the sample uses no permit. The sample shares the query memory and spill budgets, so queries that exhaust them can make it read `slow`. Read saturation from `trawl_query_permits_in_use` and `trawl_query_pool_size` on [`/metrics`](#prometheus-metrics). The response adds no sample age or diagnostics; `trawl_engine_sample_age_seconds` reports the age.

`ingest_capacity` reads `refusing` while the hot buffer refuses ingest for lack of space, and `ok` otherwise, including on a node with ingest disabled. Reads keep working while it refuses, so the response stays 200 and probes do not restart the server. See [hot-buffer admission](/architecture/data-flow/#hot-buffer-admission).

`corpus` names why corpus reads answer 503 [`corpus_recovering`](#corpus-recovering), and reads `ok` when they do not. `rollup_pending` wins when both reasons hold. The state clears without a restart, so the response stays 200. A query-only node never reads `restart_backlog`. See [reads while the corpus is unsettled](/architecture/data-flow/#reads-while-the-corpus-is-unsettled).

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
  -d '{"query": "_severity>=error last=1h | stats count(), max(bytes) as bytes by service"}' \
  "$TRAWL_URL/api/v1/query"
```

**Response**

```json
{
  "columns": [
    { "name": "service", "type": "string" },
    { "name": "count", "type": "integer" },
    { "name": "bytes", "type": "integer" }
  ],
  "rows": [["nginx", "12", "18446744073709551615"], ["api", "3", "9007199254740993"]],
  "execution": { "started_at": "2026-09-15T12:34:56.123Z", "duration_ms": 80 },
  "pagination": { "limit": 100000, "offset": 0, "returned": 2, "total": 2 },
  "degraded_fields": ["duration"],
  "severity_columns": ["sev"]
}
```

| Field | Type | Description |
|-------|------|-------------|
| `columns` | array | The columns in result order, each with its `name` and `type`. See [Result columns](#result-columns). |
| `rows` | array | One array per row, one cell per column, each encoded under its column's type |
| `pagination` | object | `limit`, `offset`, `returned`, and `total` for this page |
| `pagination.total` | integer | Rows this execution produced, before the response window was cut from them. Compare it with `returned` to tell a whole result from a page of one. |
| `execution` | object | Optional execution metadata. Present on successful server query responses, including zero-row results. Absent on synthetic client results. |
| `execution.started_at` | string | Server start instant in RFC 3339 UTC, with a `Z` suffix. Independent of the request's `timezone`. |
| `execution.duration_ms` | integer | Server-measured duration in milliseconds. Zero is valid. |
| `degraded_fields` | array | Fields the query bound whose pin is degraded, including fields filtered on and then projected away. Absent when empty. Stale by at most `schema_cache_ttl_secs`. |
| `severity_columns` | array | Result columns that hold OTel severity numbers other than `_severity`. Absent when empty. |

The server captures `execution.started_at` beside its monotonic timer. The duration includes admission and execution within that timed section. It excludes subsequent history processing, response serialization, and browser transit. Each page request has its own execution metadata.

**Errors**

| Status | Code | When |
|--------|------|------|
| 400 | `parse_error` | The DSL does not parse |
| 400 | `validation_error` | Unknown function, wrong arity, or a pipeline over the expression budget or stage cap |
| 400 | `bad_request` | `timezone` is not `UTC`, `local`, or a fixed offset, or `offset + limit` exceeds `max_result_rows` |
| 400 | `result_too_large` | The result exceeded `max_result_rows`, or an aggregation after a Rust stage such as `extract kv` would drop input at its group, distinct-value or exact-value cap. The message names the cap and its limit. |
| 400 | `execution_error` | A result cell is an integer outside the signed and unsigned 64-bit range. The message names the column. |
| 404 | `not_found` | A `from saved` stage names a saved query the key does not own, or one with no successful run |
| 409 | `bad_request` | A `from saved` stage selects a run that has columns but no Parquet file. The message names the run route. |
| 500 | `execution_error` | DuckDB failed or the query was cancelled |
| 503 | `query_resource_exhausted` | The query went over the spill or memory budget. See [query resource exhausted](#query-resource-exhausted). |
| 503 | `service_unavailable` | The deadline passed before the query started, or a cold file moved during the read. Retry. |
| 503 | `service_unavailable` | The hot buffer's snapshot could not be built. The message is `recent events could not be read for this query`. |
| 503 | `corpus_recovering` | The corpus is unsettled. See [corpus recovering](#corpus-recovering). |
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

Runs a query and returns the rows as a file. Timestamps are UTC. The row cap is `[server] max_export_rows` (`1000000`) instead of `max_result_rows`. Rows past the cap are dropped; the export is not refused.

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

An export file is not the [result wire](#result-columns). Each format keeps its own conventions:

- `json` writes one object per row. Integers are JSON numbers with every digit. A float that is not finite is `null`. Timestamps are RFC 3339 text.
- `csv` writes every cell as text. Integers keep every digit.
- `parquet` keeps DuckDB's column types. A stored `BIGINT` past 2^53 exports exactly. DuckDB writes a `HUGEINT` column as `DOUBLE`, which is exact only up to 2^53. A `sum` of integers is a `HUGEINT`.

**Errors**

| Status | Code | When |
|--------|------|------|
| 400 | `parse_error` | The DSL does not parse |
| 400 | `validation_error` | Semantic check failed |
| 400 | `result_too_large` | An `extract kv` pipeline read more than `max_export_rows` input rows, or its aggregation would drop input at its group, distinct-value or exact-value cap. The message names the cap and its limit. |
| 400 | `bad_request` | `format` is not `csv`, `json`, or `parquet`. The envelope's message quotes the request's reason. |
| 500 | `execution_error` | DuckDB failed |
| 503 | `query_resource_exhausted` | The export went over the spill or memory budget. See [query resource exhausted](#query-resource-exhausted). |
| 503 | `service_unavailable` | The deadline passed before the export started |
| 503 | `service_unavailable` | The hot buffer's snapshot could not be built. The message is `recent events could not be read for this query`. |
| 503 | `corpus_recovering` | The corpus is unsettled. See [corpus recovering](#corpus-recovering). |
| 504 | `timeout` | The deadline passed after the export started |

## Captured results

A capture runs one query once and keeps its result as an immutable, typed file that expires. Later reads page the stored rows and never run the query again. So row 7 of a capture is the same row a minute later, while new events keep arriving and compaction moves files. A captured row is the pair of a result `id` and a 0-based row number. It names a row of a frozen copy, not an event. [ADR-0057](https://github.com/jakub/trawl/blob/main/docs/adr/0057-a-capture-is-an-immutable-expiring-result-that-later-reads-page.md) records the decision.

Every route in this section needs the `query` permission. A key reads only the captures it made: another key's capture answers 404 `not_found`, as an unknown id does. The exception is [kept evidence](#kept-evidence), which every key with `investigation_read` reads. The routes use the regular [request count](/reference/configuration/#the-request-limit) and the interactive [rate limit](#rate-limiting). A page or cell read answers 400 `bad_request` to a query parameter it does not take, or to one that does not read as its type.

The numbers in this section are defaults. The [`[capture]`](/reference/configuration/#capture) table sets each of them.

### Capture a result

`POST /api/v1/results`

Permission: `query`

Runs one query under every guard that [run a query](#run-a-query) applies, and keeps its result: the key's permission and rate, the request count, an executor permit, the publication guard, the [query scratch](/reference/configuration/#query-scratch-and-budgets) budgets, history, and the [`query_timing` event](/architecture/reports-telemetry/#query-timing). A capture of the corpus answers 503 `corpus_recovering` while the corpus is unsettled. While it runs, the capture is listed by [`GET /api/v1/queries`](#active-recent-and-retained-work) with the `own` flag, and as `retained` work with the kind `capture` when it outlives its request. Its [history](#query-history) entry holds the executed text. It writes one `query_timing` event with the kind `capture`, which measures the file write in its own phase, `query_write_us`.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `query` | body | string | yes | The DSL query |
| `window` | body | object | no | The time window, instead of time clauses in `query`. See [bounds](#bounds) |
| `window.earliest` | body | string | no | The inclusive start, as an RFC 3339 instant such as `2026-10-23T09:00:00Z`. An offset is applied |
| `window.latest` | body | string | no | The exclusive end, as an RFC 3339 instant |
| `window.last` | body | string | no | A span that ends now: a number and one of `s`, `m`, `h`, `d`, or `w`, such as `1h` |

The body and `window` refuse a member they do not know, so a misspelled `window` cannot capture the default window by accident.

**Request**

```json
{
  "query": "service=nginx status>=500",
  "window": { "last": "1h" }
}
```

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" -H "Content-Type: application/json" \
  -d '{"query": "service=nginx status>=500", "window": {"last": "1h"}}' \
  "$TRAWL_URL/api/v1/results"
```

**Response**

```json
{
  "result": {
    "id": "4f2a9c1e7b3d40e8a6c5f1d2e3b4a597",
    "query": "service=nginx status>=500",
    "executed_query": "earliest=\"2026-10-23T09:00:00.000000Z\" latest=\"2026-10-23T10:00:00.000000Z\" service=nginx status>=500\n| sort -_time, -_ingested",
    "source": "live",
    "shape": "events",
    "earliest": "2026-10-23T09:00:00.000000Z",
    "latest": "2026-10-23T10:00:00.000000Z",
    "created_at": "2026-10-23T10:00:01.482113Z",
    "expires_at": "2026-10-23T10:15:01.482113Z",
    "state": "temporary",
    "columns": [
      { "name": "_time", "type": "timestamp", "stored_as": "native" },
      { "name": "_ingested", "type": "timestamp", "stored_as": "native" },
      { "name": "service", "type": "string", "stored_as": "native" },
      { "name": "status", "type": "integer", "stored_as": "native" },
      { "name": "_raw", "type": "string", "stored_as": "native" }
    ],
    "captured_rows": 5000,
    "total_rows": 18342,
    "wire_bytes": 1048311,
    "file_bytes": 212480,
    "truncated": true,
    "column_facts": [
      { "nulls": 0, "distinct": 6, "distinct_capped": true, "min": { "type": "string", "value": "2026-10-23T09:41:12.004518Z" }, "max": { "type": "string", "value": "2026-10-23T09:59:59.873002Z" } },
      { "nulls": 0, "distinct": 6, "distinct_capped": true },
      { "nulls": 0, "distinct": 1, "distinct_capped": false, "values": [{ "value": { "type": "string", "value": "nginx" }, "count": 5000 }] },
      { "nulls": 0, "distinct": 3, "distinct_capped": false, "values": [{ "value": { "type": "integer", "value": "502" }, "count": 4112 }, { "value": { "type": "integer", "value": "500" }, "count": 851 }, { "value": { "type": "integer", "value": "504" }, "count": 37 }] },
      { "nulls": 0, "distinct": 6, "distinct_capped": true }
    ],
    "format_version": 1
  },
  "execution": { "started_at": "2026-10-23T10:00:00.000141Z", "duration_ms": 1482 }
}
```

The window matched 18,342 events. The capture kept the 5,000 most recent and says so in `truncated`.

A capture attached to an investigation (ADR-0059) also answers `step_seq`, an integer: the sequence of the step the capture appended to the investigation. An unattached capture's answer has no `step_seq`.

Every capture, list, and page answer carries the result's metadata in this shape:

| Field | Type | Description |
|-------|------|-------------|
| `id` | string | The result id: 32 lowercase hex digits, a random 128-bit value that is never reused |
| `query` | string | The query text as the caller sent it |
| `executed_query` | string | The text that ran: the query with the resolved bounds spliced in and, for the `events` shape, the imposed `sort -_time, -_ingested`. A [query](#run-a-query) of this text reads the same window |
| `source` | string | `live` for the corpus, read inside the bounds. `stored` for a `from saved` query, which reads a stored report run and has no bounds |
| `shape` | string | `events` or `whole`. See [shapes and caps](#shapes-and-caps) |
| `earliest`, `latest` | string | The resolved half-open window `[earliest, latest)`, with microseconds. Absent for a `stored` source |
| `created_at` | string | When the result was committed |
| `expires_at` | string | When the result stops being readable: `created_at` plus `[capture] ttl_secs` (`900`). Absent for a `kept` result |
| `state` | string | `temporary` while it is readable. `kept` for [kept evidence](#kept-evidence) of an investigation. A gone result is `expired` or `pruned` |
| `columns` | array | The columns in stored positional order, each a [result column](#result-columns) with `stored_as`. Names are not unique |
| `columns[].stored_as` | string | `native` when the file stores the column in its own type. `wire_text` when no single file type holds it: a `mixed` column, or an integer column that holds both a negative value and one above `9223372036854775807`. Reads answer both the same way |
| `captured_rows` | integer | Rows the file holds |
| `total_rows` | integer | Rows the executed query produced before the capture cut, exact, from the same execution |
| `wire_bytes` | integer | Bytes of the captured cells as the [result columns](#result-columns) encoding writes them, without framing. The byte cap counts this |
| `file_bytes` | integer | Bytes of the stored file. `0` for a result with no columns, which has no file |
| `truncated` | boolean | `true` when `captured_rows` is less than `total_rows` |
| `column_facts` | array | Facts about each column over the captured rows, in the order of `columns` |
| `column_facts[].nulls` | integer | Captured rows whose cell is null |
| `column_facts[].distinct` | integer | Distinct non-null values, counted up to 6 |
| `column_facts[].distinct_capped` | boolean | `true` when counting stopped at 6, so `distinct` is a lower bound |
| `column_facts[].values` | array | Each distinct value with its count, most frequent first. Present only when the column holds at most 5 distinct values and each value, as a tagged cell, is at most 256 bytes |
| `column_facts[].min`, `column_facts[].max` | object | The earliest and latest instant. Present for a `_time` column of type `timestamp` only, and absent when it holds an infinite or unordered instant |
| `format_version` | integer | The layout version of the stored file |
| `investigation_id` | string | The investigation that keeps the result. Present for a `kept` result, and for one whose retention as evidence ended |

A fact value is a tagged cell, as a `mixed` column writes one: `{"type": "integer", "value": "502"}`. Text is tagged `string` whatever the column's type, so a `_time` instant reads `{"type": "string", "value": "..."}`. The response's `execution` has the meaning it has on [run a query](#run-a-query).

#### Bounds

A live capture covers one window, which the server resolves once, at the instant `T` it reads its clock, in microseconds:

- The window comes from one source: `window`, or the query's own `last=`, `earliest=`, and `latest=`. A request with both is refused.
- `last=d` covers `[T − d, T)`.
- `earliest=X` covers from `X` to `latest=Y` when given, and to `T` otherwise.
- `latest=` alone is refused, because it names no start. Add `earliest=`, or give `last=` instead.
- With neither, the capture covers the last 15 minutes, `[capture] default_window_secs`.
- `last` beside `earliest` or `latest` is refused.
- An empty window and a window longer than 7 days, `[capture] max_span_secs`, are refused.

In the query text, an `earliest=` or `latest=` value is read as [run a query](#run-a-query) reads it, which drops a spelled offset, so write the instant in UTC. In `window`, an offset is applied. A value that names no finite instant is refused. A time clause must sit where the search takes it as the window: at the top level, under `OR`, inside parentheses, or directly under `NOT`. A clause inside `NOT (...)` filters nothing in a query, so a capture refuses it.

The server cuts the query's own time clauses out of the text, writes the window in front as `earliest="..." latest="..."` with microseconds, and checks that the rest of the text is unchanged. The result is `executed_query`. A text where that check fails is refused with a request to write the time clause at the top level. Every refusal in this list is 400 `bad_request`, and comes before the capture takes anything.

A query whose first stage is `from saved` reads a stored report run, not the corpus. It takes no window and no time clause, and either is refused. It runs as written. Its result has `source: "stored"`, no `earliest` or `latest`, and the `whole` shape.

#### Shapes and caps

A capture keeps at most 5,000 rows, `[capture] max_rows`, and at most 4 MiB of cells, `[capture] max_wire_bytes`. The byte cap counts each cell as the [result columns](#result-columns) encoding writes it, without the commas and brackets between cells. The engine's own `max_result_rows` refusal applies first, with 400 `result_too_large`.

A result has the `events` shape when every stage after the search only filters events or adds fields: `where`, `extract`, `let`, `drop`, and `rename`, where no `let`, `drop`, or `rename` writes, removes, or renames `_time` or `_ingested`. The result must also carry both columns. The server appends `| sort -_time, -_ingested` to the executed text of every `events` capture, so the rows are newest first. That stage counts against the stage cap of 128, so the query of an `events` capture may carry at most 127 stages. A query with 128 is refused with 400 `bad_request` before the capture takes anything. When the rows pass a cap, the capture keeps the rows before the first one that does not fit, and `total_rows` counts every row. A first row larger than the byte cap leaves zero rows.

Every other result has the `whole` shape: one that aggregates, projects, orders, limits, deduplicates, or samples, such as `stats`, `table`, `sort`, `head`, `top`, or `dedup`, and every `from saved` result. A prefix of such a result answers a different question, so a `whole` result past either cap is refused with 400 `result_too_large`. The message names the cap and says to aggregate further or narrow the window.

A stored file larger than 8 MiB, `[capture] max_file_bytes`, is refused with `result_too_large` too.

A result with columns and no rows is stored as a file that holds only its columns. A result with no columns, such as a search that matches no event, has no file, and its `file_bytes` is `0`.

#### Admission and quotas

A capture is admitted before it takes a query id, an executor permit, or the publication guard. A refusal leaves nothing taken, and the capture appears in neither `/api/v1/queries` nor history. It still writes one [`query_timing` event](/architecture/reports-telemetry/#query-timing) with the kind `capture`. A [bounds](#bounds) refusal has `outcome=error` and `work_started=false`, and carries `query_dsl_check_us` but no `query_pool_wait_us`. A refusal at the gates below has `outcome=capacity_refused` and `work_started=false`. The checks run in this order:

1. **The key's quota, read ahead.** A key that is already full is refused before it waits.
2. **The key's lane.** One capture per key runs at a time, `[capture] per_key_running`. Up to 4 more wait in the key's queue, `[capture] queue_depth`, inside their own deadline. The next one is refused with 429 `capture_busy`.
3. **The installation.** Two captures run at once, `[capture] installation_running`, inside the executor pool. A capture that finds both running is refused with 429 `capture_busy`. It does not queue.
4. **The binding reservation.** The server first expires every temporary capture that is due and removes its file. It then reserves one result and 8 MiB, `[capture] max_file_bytes`, against the key's quota and the installation's quota, or refuses with 429 `capture_quota_exceeded`.

| Quota | Key | Default |
|-------|-----|---------|
| Live captures per key | `per_key_results` | `64` |
| File bytes per key | `per_key_bytes` | 64 MiB |
| Live captures per installation | `installation_results` | `512` |
| File bytes per installation | `installation_bytes` | 512 MiB |

A capture needs `max_file_bytes` of free quota to start, even when its file turns out small. When its row commits, the reservation becomes the file's real size.

Both 429 answers carry `Retry-After`, in whole seconds and never less than 1. For `capture_busy` it is the time until the first running capture's deadline. For `capture_quota_exceeded` it is the time until the first temporary result in the full quota expires, at most `[capture] ttl_secs`, and `ttl_secs` when none will expire. A capture whose deadline passes during admission, in the key's queue or in the store calls that expire due captures and reserve quota, gets `capture_busy` with `Retry-After: 1`. A deadline that passes later is a 503 or a 504, as the outcomes table says.

A capture reads the stored files that a [query](#query) of its recorded text reads. Before it scans, the server measures the files among them that can hold an event of the window, under the publication guard. A file is not counted when its footer statistics put every row group's `_time` outside the window, because a sender's clock can put an event's time far from the hour it was stored. A file whose footer cannot show that for every row group, such as a file with a row group that has no `_time` statistics, is counted whole. A measure larger than 1 GiB, `[capture] source_max_bytes`, is refused with 400 `capture_source_too_large`. The message gives both byte counts and nothing else. The hot buffer is not counted. A `from saved` capture measures its run's files against the same limit, and counts a missing file as 0 bytes.

A capture has one deadline of 15 seconds, `[capture] deadline_secs`, capped by `[server] timeout_secs`. It covers resolution, the wait in the queue, execution, and the file write. Past the deadline the capture ends as a [query](#query-timeout) does. A client that disconnects cancels the work, as it cancels a query. The lane slot, the installation slot, and the reservation stay charged until the work has stopped.

A capture writes its file to the [results root](/reference/configuration/#the-results-root) before it commits the result's record, so a reader never finds a record whose file is still being written. The server commits the record even when the client has gone.

**Errors**

| Status | Code | When |
|--------|------|------|
| 400 | `parse_error` | The query does not parse. The spans point into the text as sent |
| 400 | `validation_error` | Unknown function, wrong arity, or a pipeline over the expression budget or stage cap |
| 400 | `bad_request` | A [bounds](#bounds) refusal: a window and a time clause together, `latest=` alone, an empty window, a span over `max_span_secs`, a value that names no instant, a nested time clause, or a window or time clause on `from saved`. Also an `events` query of 128 stages, which leaves no stage for the [imposed order](#shapes-and-caps) |
| 400 | `result_too_large` | The result exceeded `max_result_rows`, a `whole` result exceeded `max_rows` or `max_wire_bytes`, or the file exceeded `max_file_bytes` |
| 400 | `capture_source_too_large` | The window, or the `from saved` run, selects more than `source_max_bytes` of stored data |
| 400 | `execution_error` | A result cell is an integer outside the signed and unsigned 64-bit range |
| 403 | `forbidden` | The key lacks `query` |
| 404 | `not_found` | A `from saved` stage names a saved query the key does not own, or one with no successful run |
| 409 | `bad_request` | A `from saved` stage selects a run that has columns but no Parquet file |
| 429 | `capture_busy` | The key's queue is full, two captures already run, or the deadline passed during admission: in the key's queue, or in the store reads and writes that expire due captures and reserve quota. Sent with `Retry-After`, which is `1` for the deadline case |
| 429 | `capture_quota_exceeded` | The key's or the installation's quota cannot take one more result. Sent with `Retry-After` |
| 500 | `execution_error` | DuckDB failed or the capture was cancelled |
| 503 | `query_resource_exhausted` | The query went over the spill or memory budget |
| 503 | `service_unavailable` | The deadline passed after admission and before the work started: in the wait for an executor permit or the publication guard, or in the hand-off to the worker. The message is `server at capacity: the query was not started`. Nothing stays charged |
| 503 | `service_unavailable` | The metadata store did not answer |
| 503 | `corpus_recovering` | The corpus is unsettled. See [corpus recovering](#corpus-recovering) |
| 504 | `timeout` | The deadline passed after the work started, including while the file was being written. The work keeps its slots and its reservation until it stops, and a file it had not yet published is removed |

### List captured results

`GET /api/v1/results`

Permission: `query`

Lists the key's live captures, newest first: every temporary capture before its `expires_at`. A client that lost a capture's response finds the result here. The list has no paging. It holds at most `[capture] per_key_results` entries.

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/results"
```

**Response**

```json
{
  "results": [
    { "id": "9b07d3e2c4a1458f8e6d2b1a0c9f7e63", "query": "service=nginx last=1h | stats count() by status", "executed_query": "earliest=\"2026-10-23T09:02:10.000000Z\" latest=\"2026-10-23T10:02:10.000000Z\" service=nginx  | stats count() by status", "source": "live", "shape": "whole", "earliest": "2026-10-23T09:02:10.000000Z", "latest": "2026-10-23T10:02:10.000000Z", "created_at": "2026-10-23T10:02:10.391207Z", "expires_at": "2026-10-23T10:17:10.391207Z", "state": "temporary", "columns": [{ "name": "status", "type": "integer", "stored_as": "native" }, { "name": "count", "type": "integer", "stored_as": "native" }], "captured_rows": 3, "total_rows": 3, "wire_bytes": 34, "file_bytes": 1206, "truncated": false, "column_facts": [{ "nulls": 0, "distinct": 3, "distinct_capped": false, "values": [{ "value": { "type": "integer", "value": "200" }, "count": 1 }, { "value": { "type": "integer", "value": "404" }, "count": 1 }, { "value": { "type": "integer", "value": "502" }, "count": 1 }] }, { "nulls": 0, "distinct": 3, "distinct_capped": false, "values": [{ "value": { "type": "integer", "value": "48211" }, "count": 1 }, { "value": { "type": "integer", "value": "1290" }, "count": 1 }, { "value": { "type": "integer", "value": "4112" }, "count": 1 }] }], "format_version": 1 }
  ]
}
```

Each entry has the fields of a [captured result](#capture-a-result).

**Errors**

| Status | Code | When |
|--------|------|------|
| 403 | `forbidden` | The key lacks `query` |
| 503 | `service_unavailable` | The metadata store did not answer |

### Read a page of a captured result

`GET /api/v1/results/{id}`

Permission: `query`

Returns rows of a capture by row number, from the stored file. It runs no query and takes no executor permit.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `id` | path | string | yes | The result id |
| `offset` | query | integer | no | The first row number, 0-based. Default `0`. At most `captured_rows`; an `offset` equal to `captured_rows` is accepted and returns a page with no rows |
| `limit` | query | integer | no | Rows in the page. Default and cap: `[capture] page_max_rows` (`200`). `0` is accepted and returns a page with no rows |
| `columns` | query | string | no | Column names separated by commas. Default: every column |

A name in `columns` selects every column with that name, so a duplicate name returns each of its columns. The page carries the selected columns in stored order, whatever order `columns` names them in. A name that the result does not have is 400 `bad_request`. An empty `columns=` names one column called `''`, so it is refused the same way unless the result has a column with an empty name. A name that holds a comma cannot be selected on its own; leave `columns` out to read every column.

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" \
  "$TRAWL_URL/api/v1/results/9b07d3e2c4a1458f8e6d2b1a0c9f7e63?offset=1&limit=2&columns=count"
```

**Response**

```json
{
  "result": {
    "id": "9b07d3e2c4a1458f8e6d2b1a0c9f7e63",
    "query": "service=nginx last=1h | stats count() by status",
    "executed_query": "earliest=\"2026-10-23T09:02:10.000000Z\" latest=\"2026-10-23T10:02:10.000000Z\" service=nginx  | stats count() by status",
    "source": "live",
    "shape": "whole",
    "earliest": "2026-10-23T09:02:10.000000Z",
    "latest": "2026-10-23T10:02:10.000000Z",
    "created_at": "2026-10-23T10:02:10.391207Z",
    "expires_at": "2026-10-23T10:17:10.391207Z",
    "state": "temporary",
    "columns": [
      { "name": "status", "type": "integer", "stored_as": "native" },
      { "name": "count", "type": "integer", "stored_as": "native" }
    ],
    "captured_rows": 3,
    "total_rows": 3,
    "wire_bytes": 34,
    "file_bytes": 1206,
    "truncated": false,
    "column_facts": [
      { "nulls": 0, "distinct": 3, "distinct_capped": false, "values": [{ "value": { "type": "integer", "value": "200" }, "count": 1 }, { "value": { "type": "integer", "value": "404" }, "count": 1 }, { "value": { "type": "integer", "value": "502" }, "count": 1 }] },
      { "nulls": 0, "distinct": 3, "distinct_capped": false, "values": [{ "value": { "type": "integer", "value": "48211" }, "count": 1 }, { "value": { "type": "integer", "value": "1290" }, "count": 1 }, { "value": { "type": "integer", "value": "4112" }, "count": 1 }] }
    ],
    "format_version": 1
  },
  "positions": [1],
  "page": {
    "columns": [{ "name": "count", "type": "integer" }],
    "rows": [["1290"], ["4112"]]
  },
  "pagination": { "limit": 2, "offset": 1, "returned": 2, "total": 3 }
}
```

| Field | Type | Description |
|-------|------|-------------|
| `result` | object | The [captured result](#capture-a-result) the page belongs to |
| `positions` | array | For each column of `page`, its 0-based position in `result.columns`, ascending |
| `page` | object | The selected columns and the page's rows, encoded as [result columns](#result-columns). Row `i` of the page is captured row `offset + i` |
| `pagination.total` | integer | The result's `captured_rows` |

A result with no columns answers rows that are empty arrays and reads no file. Two reads of the same rows of one capture return the same bytes, whatever ingest, compaction, or a restart did between them.

**Errors**

| Status | Code | When |
|--------|------|------|
| 400 | `bad_request` | `limit` is above `page_max_rows`, `offset` is past `captured_rows`, `columns` names a column the result does not have, or a query parameter is unknown or not an integer |
| 403 | `forbidden` | The key lacks `query` |

Every [captured result outcome](#captured-result-outcomes) applies too.

### Read one cell of a captured result

`GET /api/v1/results/{id}/cells/{row}/{column}`

Permission: `query`

Returns a byte range of one cell, so a client can read a cell too long for a page. The range is over the cell's text as the [result columns](#result-columns) encoding writes it, under the column's type. A string cell's text is a JSON string, quotes included.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `id` | path | string | yes | The result id |
| `row` | path | integer | yes | The row number, 0-based |
| `column` | path | integer | yes | The column's position in `columns`, 0-based. Names are not unique, so a cell is addressed by position |
| `offset` | query | integer | no | The first byte of the range. Default `0`. It must start a UTF-8 character: use `0` or a `next_offset` |
| `length` | query | integer | no | The most bytes in the range, at least 4. Default and cap: `[capture] max_wire_bytes`, which holds any captured cell whole |

The range ends at a character boundary, so it can be shorter than `length`. Follow `next_offset` until it is absent, join the `text` values, and decode the joined text as a cell of the column's type.

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" \
  "$TRAWL_URL/api/v1/results/4f2a9c1e7b3d40e8a6c5f1d2e3b4a597/cells/0/4?length=64"
```

**Response**

```json
{
  "type": "string",
  "offset": 0,
  "length": 64,
  "total_bytes": 109,
  "next_offset": 64,
  "text": "\"203.0.113.7 - - [23/Oct/2026:09:59:59 +0000] \\\"GET /api/orders/"
}
```

The next request asks for `offset=64` and gets the rest of the cell, with no `next_offset`:

```json
{
  "type": "string",
  "offset": 64,
  "length": 45,
  "total_bytes": 109,
  "text": "8812 HTTP/1.1\\\" 502 157 \\\"-\\\" \\\"curl/8.9.1\\\"\""
}
```

| Field | Type | Description |
|-------|------|-------------|
| `type` | string | The column's type, spelled as a [result column](#result-columns) spells it. A `list` type also has `items` |
| `offset` | integer | The byte offset of `text` in the cell's text |
| `length` | integer | The byte length of `text` |
| `total_bytes` | integer | The byte length of the whole cell's text |
| `next_offset` | integer | The offset of the next range. Absent when `text` reaches the end |
| `text` | string | The range of the cell's text |

**Errors**

| Status | Code | When |
|--------|------|------|
| 400 | `bad_request` | `row` is past the captured rows, `column` is past the columns, `offset` is past the cell or inside a character, `length` is below 4 or above `max_wire_bytes`, or a path segment or query parameter does not read as an integer |
| 403 | `forbidden` | The key lacks `query` |

Every [captured result outcome](#captured-result-outcomes) applies too.

### Captured result outcomes

A page or cell read checks, in this order, before it reads any row:

| Status | Code | When |
|--------|------|------|
| 404 | `not_found` | The id is not 32 lowercase hex digits, no capture has it, or another key made it and it is not [kept evidence](#kept-evidence) the key may read |
| 410 | `result_gone` | The capture passed its `expires_at`, was pruned to fit a lowered quota, or its investigation's evidence retention ended or the investigation was deleted. The message says which. The first two ask for a new capture; an ended retention says the findings that cite the result remain. Only the result's owner gets the deleted-investigation answer; another key gets `not_found` |
| 404 | `not_found` | The capture was gone for longer than `[capture] tombstone_secs` (`86400`), counted from the moment it was lost |
| 409 | `result_unavailable` | The record exists, but its file is missing. The response is never an empty page, and the query is never run again |
| 500 | `internal_error` | The record names a file format version that this server does not read |
| 500 | `result_corrupt` | The file does not match the blake3 digest recorded at capture. The server checks the whole file on every read. The response names no path, and the server logs a `result_corrupt` event with the result id beside the request's [`http_failure` event](/operate/health/#trace-a-server-failure) |
| 503 | `service_unavailable` | The metadata store did not answer |

A gone capture keeps a record, called a tombstone, so that its owner can tell expired from never existed. A read that finds a temporary capture past its `expires_at` marks it expired and removes its file at once. The sweep removes the rest: every hour with the [scheduler](/reference/configuration/#scheduler) on, and every `[capture] ttl_secs`, at most hourly and at least every second, with it off. The sweep also deletes tombstones past `tombstone_secs`. A capture survives a restart until its `expires_at`. [Back up and restore](/operate/backup-restore/) describes what the start does to captures and their files.

## Investigations

An investigation is a titled question that the installation keeps while an agent or a person works on it. It holds a log: the steps the server ran for it, the rows kept as evidence, findings, and notes. The installation owns it, not a key. Every key that holds `query` and `investigation_read` reads every investigation, so an agent and a person can work in the same one under different keys. [ADR-0059](https://github.com/jakub/trawl/blob/main/docs/adr/0059-an-investigation-is-grant-visible-and-its-evidence-outlives-the-corpus.md) records the decision. The [`investigation` tool](/reference/mcp/#investigation) of `trawl mcp` calls these routes.

Each route needs more than one permission:

| Authority | Permissions | Routes |
|-----------|-------------|--------|
| Read | `query` and `investigation_read` | List, read, and the [kept evidence](#kept-evidence) reads |
| Write | Read, and `investigation_write` | Create, append, overwrite, and [attach a capture](#attach-a-capture-to-an-investigation) |
| Manage | Write, and `server_manage` | Extend retention and delete |

A key with `server_manage` alone cannot extend or delete: it needs the write permissions too. trawld checks the authority before it reads the path, the query string, or the body. So a key without it gets 403 `forbidden` for any request, and a key with it gets 400 `bad_request` for a request that does not read as its type. [Attach a capture](#attach-a-capture-to-an-investigation) is the exception. Its route, `POST /api/v1/results`, needs `query`, and trawld checks `query` before it reads the body. Only the body says that a capture names an investigation, so trawld checks Write authority after it reads the body and before it writes anything. A key with `query` but without Write authority gets 400 `bad_request` for a body that does not read as a capture, and 403 `forbidden` for one that does. Any writer can close or reopen any investigation. The creator's identity is attribution and grants nothing.

The routes use the regular [request count](/reference/configuration/#the-request-limit) and the interactive [rate limit](#rate-limiting). Timestamps are RFC 3339 UTC with microseconds.

**Ids.** An investigation id is 32 lowercase hex digits, a random 128-bit value. The client mints it, not the server, so a create whose response was lost can be sent again. A path id in another shape is 400 `bad_request`. An id that names no investigation is 404 `not_found` with the message `investigation not found`.

**Actors.** Every investigation and every entry names who wrote it as an actor:

| Field | Type | Description |
|-------|------|-------------|
| `prefix` | string | The key's 8-character prefix, as [current key](#current-key) prints it. It never changes |
| `name` | string | The key's name when it wrote |

Both are copies taken at the write. They still read after the key is renamed, expires, or is revoked. An actor never carries the key's internal id.

**Sequences.** Every write advances the investigation's `seq` by one. Each entry records `seq`, the sequence that created it, and `updated_seq`, the sequence of its latest change. Only a step changes after it is created, when its outcome is written. A finding's `seq` is its id. Creation is sequence 0 and writes no entry.

Appends (keep, finding, note) carry no expected sequence. Two writers who append at the same time both succeed. Overwrites (title, question, status, finding status) and a retention extension carry `expected_seq`, the `seq` the caller last saw. When the investigation's `seq` has moved, trawld refuses with 409 `sequence_conflict`, and the envelope's `conflict` member holds both numbers:

```json
{
  "error": {
    "code": "sequence_conflict",
    "message": "the investigation changed: the request expected sequence 7 and it is now 8; read it again and retry",
    "conflict": { "expected": 7, "current": 8 }
  }
}
```

Read the investigation again, then retry with the new `seq`.

**Status.** An investigation is `open` or `closed`. A closed investigation refuses every write with 409 `investigation_closed`, except a reopen through [overwrite](#overwrite-a-title-question-or-status) and a [retention extension](#extend-evidence-retention). Work that trawld admitted before the close still finishes and records its outcome.

**Bounds.** trawld checks each bound before it stores anything, and refuses with 400 `bad_request`. The message names the field and the limit. Lengths are bytes of UTF-8. No text field may hold a NUL character, and a required text field may not be blank.

| Field | Limit |
|-------|-------|
| `title` | 200 bytes, required |
| `question` | 2048 bytes, required |
| `scope` | 2048 bytes |
| Note `text` | 4096 bytes, required |
| Finding `statement` | 4096 bytes, required |
| Finding `remaining_question` | 1024 bytes |
| Finding `references`, `counterevidence` | 32 rows each |
| Keep `rows` | 1 to 200 row numbers |
| Keep `reason` | 1024 bytes, required |
| Attached capture `purpose` | 1024 bytes |

**Retention.** Kept evidence files have two clocks, set in [`[investigation]`](/reference/configuration/#investigation). `retain_until` is 7 days after the investigation's last write, `evidence_idle_secs`. It never passes `retain_ceiling`, 30 days after creation, `evidence_ceiling_secs`. Each write by a key slides `retain_until` forward, up to the ceiling. Reads slide nothing. Only a key with Manage authority can [extend](#extend-evidence-retention) evidence past the ceiling. When `retain_until` passes, trawld removes the evidence files and keeps their records, so a [kept row](#kept-evidence) reads as gone and the findings that cite it remain. The investigation's own text, its steps, findings, notes, and keeps, stays until an operator [deletes](#delete-an-investigation) the investigation.

A write that arrives after `retain_until` first removes the evidence, as the next sweep would, and then slides the clock. So the write never brings back evidence whose retention had ended, and the hourly sweep never decides what survives.

### List investigations

`GET /api/v1/investigations`

Authority: Read

Lists every investigation in the installation: open ones first, then closed ones, each group most recently changed first. The list holds at most 200 entries and has no paging.

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/investigations"
```

**Response**

```json
{
  "investigations": [
    {
      "id": "6d0c4f7e2a9b41c38e5f0a1b2c3d4e5f",
      "title": "502s on checkout",
      "status": "open",
      "seq": 8,
      "created_at": "2026-10-24T09:00:00.000000Z",
      "updated_at": "2026-10-24T09:41:20.000000Z",
      "created_by": { "prefix": "q7Rk2LmP", "name": "claude-agent" },
      "updated_by": { "prefix": "Hn4vX9aT", "name": "jakub" }
    },
    {
      "id": "0f9e8d7c6b5a49382716a5b4c3d2e1f0",
      "title": "Disk alerts on web-03",
      "status": "closed",
      "seq": 5,
      "created_at": "2026-10-20T14:12:09.000000Z",
      "updated_at": "2026-10-21T08:03:44.000000Z",
      "created_by": { "prefix": "Hn4vX9aT", "name": "jakub" },
      "updated_by": { "prefix": "Hn4vX9aT", "name": "jakub" }
    }
  ]
}
```

Each entry has the summary fields of an [investigation](#create-an-investigation): `id`, `title`, `status`, `seq`, `created_at`, `updated_at`, `created_by`, and `updated_by`.

**Errors**

| Status | Code | When |
|--------|------|------|
| 403 | `forbidden` | The key lacks `query` or `investigation_read` |
| 503 | `service_unavailable` | The metadata store did not answer |

### Create an investigation

`PUT /api/v1/investigations/{id}`

Authority: Write

Creates the investigation `id` if no investigation has that id. The same key that sends the same fields again gets the existing investigation back with 200, and nothing changes. That is how a client recovers a create whose response it lost. A request from another key, or with other fields, for an id that exists is refused with 409 `investigation_exists`.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `id` | path | string | yes | A new id: 32 lowercase hex digits from a random 128-bit value |
| `title` | body | string | yes | A short title |
| `question` | body | string | yes | The question the investigation answers |
| `scope` | body | string | no | What the investigation covers. Default: empty |

The body refuses a member it does not know.

**Request**

```json
{
  "title": "502s on checkout",
  "question": "Why does checkout return 502 since 09:00?",
  "scope": "env=prod, service=nginx and shop-api"
}
```

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" -X PUT -H "Content-Type: application/json" \
  -d '{"title": "502s on checkout", "question": "Why does checkout return 502 since 09:00?", "scope": "env=prod, service=nginx and shop-api"}' \
  "$TRAWL_URL/api/v1/investigations/6d0c4f7e2a9b41c38e5f0a1b2c3d4e5f"
```

**Response**

201 when this request created the investigation, 200 when it already existed with the same creator and fields.

```json
{
  "id": "6d0c4f7e2a9b41c38e5f0a1b2c3d4e5f",
  "title": "502s on checkout",
  "question": "Why does checkout return 502 since 09:00?",
  "scope": "env=prod, service=nginx and shop-api",
  "status": "open",
  "seq": 0,
  "created_at": "2026-10-24T09:00:00.000000Z",
  "updated_at": "2026-10-24T09:00:00.000000Z",
  "retain_until": "2026-10-31T09:00:00.000000Z",
  "retain_ceiling": "2026-11-23T09:00:00.000000Z",
  "created_by": { "prefix": "q7Rk2LmP", "name": "claude-agent" },
  "updated_by": { "prefix": "q7Rk2LmP", "name": "claude-agent" }
}
```

| Field | Type | Description |
|-------|------|-------------|
| `id` | string | The id |
| `title`, `question` | string | The current title and question |
| `scope` | string | What it covers. Empty when the create gave none |
| `status` | string | `open` or `closed` |
| `seq` | integer | The sequence of the latest write |
| `created_at`, `updated_at` | string | When it was created and when it last changed |
| `retain_until` | string | When its evidence files are removed, unless it changes again |
| `retain_ceiling` | string | The latest instant that `retain_until` can slide to: 30 days after creation, unless a key with Manage authority extended it |
| `created_by`, `updated_by` | object | The [actors](#investigations) who created it and who changed it last |

**Errors**

| Status | Code | When |
|--------|------|------|
| 400 | `bad_request` | The id is not 32 lowercase hex digits, `title` or `question` is blank, a field is over its bound or holds a NUL character, or the body has an unknown member or does not read as the request |
| 403 | `forbidden` | The key lacks `query`, `investigation_read`, or `investigation_write` |
| 409 | `investigation_exists` | An investigation with this id exists with another creator or other fields. Create it under a new id |
| 503 | `service_unavailable` | The metadata store did not answer |

### Read an investigation

`GET /api/v1/investigations/{id}`

Authority: Read

Returns the investigation and the entries that changed after `since`, in `updated_seq` order. A client that polls keeps the `through_seq` of each answer and sends it as the next `since`. It then receives each new entry, and each step whose outcome arrived, once.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `id` | path | string | yes | The investigation id |
| `since` | query | integer | no | Return the entries whose `updated_seq` is above this. Default `0`, every entry |
| `limit` | query | integer | no | The most entries to return, 1 to 200. Default `100` |

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" \
  "$TRAWL_URL/api/v1/investigations/6d0c4f7e2a9b41c38e5f0a1b2c3d4e5f?since=3"
```

**Response**

```json
{
  "investigation": {
    "id": "6d0c4f7e2a9b41c38e5f0a1b2c3d4e5f",
    "title": "502s on checkout",
    "question": "Why does checkout return 502 since 09:00?",
    "scope": "env=prod, service=nginx and shop-api",
    "status": "open",
    "seq": 8,
    "created_at": "2026-10-24T09:00:00.000000Z",
    "updated_at": "2026-10-24T09:41:20.000000Z",
    "retain_until": "2026-10-31T09:41:20.000000Z",
    "retain_ceiling": "2026-11-23T09:00:00.000000Z",
    "created_by": { "prefix": "q7Rk2LmP", "name": "claude-agent" },
    "updated_by": { "prefix": "Hn4vX9aT", "name": "jakub" }
  },
  "entries": [
    {
      "seq": 4,
      "updated_seq": 4,
      "actor": { "prefix": "Hn4vX9aT", "name": "jakub" },
      "at": "2026-10-24T09:12:31.000000Z",
      "payload": { "kind": "note", "text": "Deploy 4812 of shop-api went out at 08:58." }
    },
    {
      "seq": 5,
      "updated_seq": 6,
      "actor": { "prefix": "q7Rk2LmP", "name": "claude-agent" },
      "at": "2026-10-24T09:20:02.000000Z",
      "payload": {
        "kind": "step",
        "executed": "earliest=\"2026-10-24T08:20:02.000000Z\" latest=\"2026-10-24T09:20:02.000000Z\" service=shop-api level=error\n| sort -_time, -_ingested",
        "bounds": { "earliest": "2026-10-24T08:20:02.000000Z", "latest": "2026-10-24T09:20:02.000000Z" },
        "request_id": "01JBA5W2Q7Z3N8M4K6P0R9T1V2",
        "query_id": 2214,
        "purpose": "Check whether shop-api logs errors after the deploy",
        "outcome": { "outcome": "success", "result_id": "a1b2c3d4e5f60718293a4b5c6d7e8f90", "rows": 12, "duration_ms": 318 }
      }
    },
    {
      "seq": 7,
      "updated_seq": 7,
      "actor": { "prefix": "q7Rk2LmP", "name": "claude-agent" },
      "at": "2026-10-24T09:31:47.000000Z",
      "payload": {
        "kind": "finding",
        "statement": "The 502s start when shop-api begins refusing database connections after deploy 4812.",
        "status": "hypothesis",
        "references": [
          { "result_id": "4f2a9c1e7b3d40e8a6c5f1d2e3b4a597", "row": 0 },
          { "result_id": "a1b2c3d4e5f60718293a4b5c6d7e8f90", "row": 1 }
        ],
        "counterevidence": [],
        "remaining_question": "Why does the connection pool not recover?"
      }
    },
    {
      "seq": 8,
      "updated_seq": 8,
      "actor": { "prefix": "Hn4vX9aT", "name": "jakub" },
      "at": "2026-10-24T09:41:20.000000Z",
      "payload": { "kind": "finding_status", "finding_seq": 7, "status": "supported" }
    }
  ],
  "through_seq": 8,
  "more": false
}
```

| Field | Type | Description |
|-------|------|-------------|
| `investigation` | object | The investigation, with the fields of [create](#create-an-investigation) |
| `entries` | array | The entries whose `updated_seq` is above `since`, in `updated_seq` order, at most `limit` |
| `entries[].seq` | integer | The sequence that created the entry. A finding's id |
| `entries[].updated_seq` | integer | The sequence of the entry's latest change: `seq`, or later for a step whose outcome came after it |
| `entries[].actor` | object | Who wrote it. For a step, the key whose request started the work |
| `entries[].at` | string | When the entry was created |
| `entries[].payload` | object | What the entry records, tagged by `kind`. See the table below |
| `through_seq` | integer | The `since` of the next read. The investigation's `seq` when `more` is `false` |
| `more` | boolean | `true` when entries past `through_seq` remain. Read again with `since` set to `through_seq` |

| `kind` | Written by | Members |
|--------|------------|---------|
| `step` | trawld, for an [attached capture](#attach-a-capture-to-an-investigation) | `executed`, the text that ran. `bounds`, its `earliest` and `latest`, absent for a `from saved` source. `request_id`, the `X-Request-Id` of the request that started it. `query_id`, the id of its [`query_timing` event](/architecture/reports-telemetry/#query-timing). `purpose`, when the request gave one. `outcome`, absent while the work runs |
| `keep` | An [append](#append-a-keep-a-finding-or-a-note) | `result_id`, `rows`, and `reason` |
| `finding` | An append | `statement`, `status`, `references`, `counterevidence`, and `remaining_question` when given. `status` is the status the finding was written with |
| `finding_status` | A [finding status overwrite](#overwrite-a-findings-status) | `finding_seq`, the finding's `seq`, and its new `status` |
| `note` | An append | `text` |
| `status` | An [overwrite](#overwrite-a-title-question-or-status) | `status`, `open` or `closed` |
| `title` | An overwrite | `title` |
| `question` | An overwrite | `question` |

A finding's current status is the `status` of its latest `finding_status` entry, or its own `status` when it has none. A finding's status is its author's claim. trawld checks that the rows it cites exist and can be read, not that they prove the statement.

A step's `outcome` has these members:

| Field | Type | Description |
|-------|------|-------------|
| `outcome` | string | `success`, the result was kept. `refused`, the evidence budget or the capture queue refused it after the step was written. `timeout`, it ran past its deadline. `failed`, it failed. `aborted`, the client went away before the result was committed. `interrupted`, trawld stopped while it ran, and the next start recorded this |
| `code` | string | The [error code](#error-envelope) the work ended with. Absent for `success`, `aborted`, and `interrupted` |
| `result_id` | string | For `success`, the kept result |
| `rows` | integer | For `success`, the rows the result holds |
| `duration_ms` | integer | How long the work ran, when it ran |

**Errors**

| Status | Code | When |
|--------|------|------|
| 400 | `bad_request` | The id is not 32 lowercase hex digits, `limit` is outside 1 to 200, or a query parameter is unknown or not an integer |
| 403 | `forbidden` | The key lacks `query` or `investigation_read` |
| 404 | `not_found` | No investigation has this id |
| 503 | `service_unavailable` | The metadata store did not answer |

### Append a keep, a finding, or a note

`POST /api/v1/investigations/{id}/entries`

Authority: Write

Appends one entry to an open investigation. The body is tagged by `kind`: `keep`, `finding`, or `note`. An append carries no expected sequence, so it never fails because someone else wrote first.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `id` | path | string | yes | The investigation id |
| `kind` | body | string | yes | `keep`, `finding`, or `note` |
| `result_id` | body | string | for `keep` | The captured result whose rows to keep |
| `rows` | body | array | for `keep` | Row numbers of that result, counted from 0 |
| `reason` | body | string | for `keep` | Why the rows matter |
| `statement` | body | string | for `finding` | The finding |
| `status` | body | string | for `finding` | `hypothesis`, `supported`, `refuted`, or `unresolved` |
| `references` | body | array | for `finding` | The rows that support it, each `{"result_id": "...", "row": N}`. May be empty |
| `counterevidence` | body | array | no | The rows that count against it, in the same shape |
| `remaining_question` | body | string | no | What the evidence leaves open |
| `text` | body | string | for `note` | The note |
| `seen_seq` | body | integer | no | The latest `seq` the caller has seen. With it, the response carries `activity` |

The body refuses a member that its `kind` does not take.

**Request**

A keep:

```json
{
  "kind": "keep",
  "result_id": "4f2a9c1e7b3d40e8a6c5f1d2e3b4a597",
  "rows": [0, 4],
  "reason": "The first 502 and the first upstream reset, 40 ms apart"
}
```

A finding that cites rows of two results:

```json
{
  "kind": "finding",
  "statement": "The 502s start when shop-api begins refusing database connections after deploy 4812.",
  "status": "hypothesis",
  "references": [
    { "result_id": "4f2a9c1e7b3d40e8a6c5f1d2e3b4a597", "row": 0 },
    { "result_id": "a1b2c3d4e5f60718293a4b5c6d7e8f90", "row": 1 }
  ],
  "remaining_question": "Why does the connection pool not recover?",
  "seen_seq": 6
}
```

A note:

```json
{ "kind": "note", "text": "Deploy 4812 of shop-api went out at 08:58." }
```

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" -H "Content-Type: application/json" \
  -d '{"kind": "note", "text": "Deploy 4812 of shop-api went out at 08:58."}' \
  "$TRAWL_URL/api/v1/investigations/6d0c4f7e2a9b41c38e5f0a1b2c3d4e5f/entries"
```

**Response**

201, with the entry written. The finding above answers:

```json
{
  "entry": {
    "seq": 7,
    "updated_seq": 7,
    "actor": { "prefix": "q7Rk2LmP", "name": "claude-agent" },
    "at": "2026-10-24T09:31:47.000000Z",
    "payload": {
      "kind": "finding",
      "statement": "The 502s start when shop-api begins refusing database connections after deploy 4812.",
      "status": "hypothesis",
      "references": [
        { "result_id": "4f2a9c1e7b3d40e8a6c5f1d2e3b4a597", "row": 0 },
        { "result_id": "a1b2c3d4e5f60718293a4b5c6d7e8f90", "row": 1 }
      ],
      "counterevidence": [],
      "remaining_question": "Why does the connection pool not recover?"
    }
  },
  "activity": { "entries": [], "through_seq": 7, "more": false }
}
```

| Field | Type | Description |
|-------|------|-------------|
| `entry` | object | The entry written, as a [read](#read-an-investigation) returns it |
| `activity` | object | Present when the request sent `seen_seq`. The entries that other keys changed after `seen_seq`, with `entries`, `through_seq`, and `more` as a read has them, at most 50. The caller's own entries are left out. When `more` is `true`, a [read](#read-an-investigation) with `since` set to `through_seq` returns the rest |

A keep and a finding cite rows of captured results. Each cited result must be [kept evidence](#kept-evidence) of this investigation, or a temporary result that the caller's own key captured and that has not expired. A cited temporary result is kept with the entry: trawld attaches the whole result to the investigation in place, without copying its file, and charges its file bytes once to the evidence budgets. Each cited row number must be below the result's `captured_rows`.

A result that another investigation keeps is refused with 409. Every other result that cannot be cited is refused with 400 `bad_request` and the same message, `... cites result ID, which is not kept evidence of this investigation`. That covers a result that does not exist, another key's temporary result, an expired or removed result, and a temporary result whose file is missing. So the refusal says nothing that a read would not.

**Errors**

| Status | Code | When |
|--------|------|------|
| 400 | `bad_request` | The id is not 32 lowercase hex digits, a field is blank, over its bound, or holds a NUL character, a keep names no rows, a citation names a result id in another shape, a result that cannot be cited, or a row past its `captured_rows`, or the body does not read as an entry |
| 403 | `forbidden` | The key lacks `query`, `investigation_read`, or `investigation_write` |
| 404 | `not_found` | No investigation has this id |
| 409 | `investigation_closed` | The investigation is closed. Reopen it first |
| 409 | `bad_request` | A citation names a result that another investigation keeps |
| 409 | `evidence_budget_exceeded` | Keeping a cited temporary result would take this investigation's or the installation's evidence over its budget. The message gives the bytes kept and the budget. See [attach a capture](#attach-a-capture-to-an-investigation) |
| 429 | `capture_busy` | The evidence budgets could not be checked before the deadline because other captures held them, or the deadline passed while the promotion of a cited temporary result waited for a database connection, for the lock of the investigation or of a cited result, or for an answer from the database. Nothing was written. The deadline is the query timeout, `[server] timeout_secs`. Sent with `Retry-After`, which is `1` when the wait for a connection, a lock, or an answer ran out |
| 503 | `service_unavailable` | The metadata store did not answer |

### Overwrite a title, question, or status

`PATCH /api/v1/investigations/{id}`

Authority: Write

Changes one of the investigation's title, question, or status, if its `seq` is still `expected_seq`. Each change writes a `title`, `question`, or `status` entry, so the log shows who changed what. A patch carries exactly one of the three. To change two, send two patches, each with the `seq` the one before it returned. A closed investigation takes only a reopen, `"status": "open"`.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `id` | path | string | yes | The investigation id |
| `expected_seq` | body | integer | yes | The `seq` the caller last saw |
| `title` | body | string | one of three | A new title |
| `question` | body | string | one of three | A new question |
| `status` | body | string | one of three | `open` or `closed` |

**Request**

```json
{ "expected_seq": 8, "status": "closed" }
```

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" -X PATCH -H "Content-Type: application/json" \
  -d '{"expected_seq": 8, "status": "closed"}' \
  "$TRAWL_URL/api/v1/investigations/6d0c4f7e2a9b41c38e5f0a1b2c3d4e5f"
```

**Response**

The investigation after the change:

```json
{
  "id": "6d0c4f7e2a9b41c38e5f0a1b2c3d4e5f",
  "title": "502s on checkout",
  "question": "Why does checkout return 502 since 09:00?",
  "scope": "env=prod, service=nginx and shop-api",
  "status": "closed",
  "seq": 9,
  "created_at": "2026-10-24T09:00:00.000000Z",
  "updated_at": "2026-10-24T10:02:15.000000Z",
  "retain_until": "2026-10-31T10:02:15.000000Z",
  "retain_ceiling": "2026-11-23T09:00:00.000000Z",
  "created_by": { "prefix": "q7Rk2LmP", "name": "claude-agent" },
  "updated_by": { "prefix": "Hn4vX9aT", "name": "jakub" }
}
```

The response carries no `activity`, and the request takes no `seen_seq`.

**Errors**

| Status | Code | When |
|--------|------|------|
| 400 | `bad_request` | The id is not 32 lowercase hex digits, the patch names none or more than one of `title`, `question`, and `status`, a field is blank, over its bound, or holds a NUL character, or the body does not read as the request |
| 403 | `forbidden` | The key lacks `query`, `investigation_read`, or `investigation_write` |
| 404 | `not_found` | No investigation has this id |
| 409 | `investigation_closed` | The investigation is closed and the patch is not a reopen. trawld checks this before the sequence |
| 409 | `sequence_conflict` | The investigation's `seq` is no longer `expected_seq`. The `conflict` member holds both |
| 503 | `service_unavailable` | The metadata store did not answer |

### Overwrite a finding's status

`PUT /api/v1/investigations/{id}/findings/{seq}/status`

Authority: Write

Sets the status of the finding whose `seq` is `{seq}`, if the investigation's `seq` is still `expected_seq`. It writes a `finding_status` entry. The investigation must be open.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `id` | path | string | yes | The investigation id |
| `seq` | path | integer | yes | The finding's `seq` |
| `expected_seq` | body | integer | yes | The investigation's `seq` that the caller last saw |
| `status` | body | string | yes | `hypothesis`, `supported`, `refuted`, or `unresolved` |

**Request**

```json
{ "expected_seq": 7, "status": "supported" }
```

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" -X PUT -H "Content-Type: application/json" \
  -d '{"expected_seq": 7, "status": "supported"}' \
  "$TRAWL_URL/api/v1/investigations/6d0c4f7e2a9b41c38e5f0a1b2c3d4e5f/findings/7/status"
```

**Response**

```json
{
  "entry": {
    "seq": 8,
    "updated_seq": 8,
    "actor": { "prefix": "Hn4vX9aT", "name": "jakub" },
    "at": "2026-10-24T09:41:20.000000Z",
    "payload": { "kind": "finding_status", "finding_seq": 7, "status": "supported" }
  }
}
```

The response has the shape of an [append](#append-a-keep-a-finding-or-a-note) and never carries `activity`. The request takes no `seen_seq`.

**Errors**

| Status | Code | When |
|--------|------|------|
| 400 | `bad_request` | The id is not 32 lowercase hex digits, `{seq}` is not an integer, or the body does not read as the request |
| 403 | `forbidden` | The key lacks `query`, `investigation_read`, or `investigation_write` |
| 404 | `not_found` | No investigation has this id, or `{seq}` names no finding of it. The second message is `finding not found or unauthorized` |
| 409 | `investigation_closed` | The investigation is closed |
| 409 | `sequence_conflict` | The investigation's `seq` is no longer `expected_seq` |
| 503 | `service_unavailable` | The metadata store did not answer |

### Extend evidence retention

`PUT /api/v1/investigations/{id}/retention`

Authority: Manage

Keeps the investigation's evidence files until `retain_until`, past the ceiling if need be, if its `seq` is still `expected_seq`. It raises `retain_ceiling` to the new instant when the instant is later. It never shortens either one. It advances `seq` and writes no entry. It works on a closed investigation too.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `id` | path | string | yes | The investigation id |
| `expected_seq` | body | integer | yes | The `seq` the caller last saw |
| `retain_until` | body | string | yes | An RFC 3339 instant in UTC, such as `2026-12-31T00:00:00Z`, no earlier than now and no earlier than the current `retain_until` |

**Request**

```json
{ "expected_seq": 9, "retain_until": "2026-12-31T00:00:00Z" }
```

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" -X PUT -H "Content-Type: application/json" \
  -d '{"expected_seq": 9, "retain_until": "2026-12-31T00:00:00Z"}' \
  "$TRAWL_URL/api/v1/investigations/6d0c4f7e2a9b41c38e5f0a1b2c3d4e5f/retention"
```

**Response**

```json
{
  "id": "6d0c4f7e2a9b41c38e5f0a1b2c3d4e5f",
  "title": "502s on checkout",
  "question": "Why does checkout return 502 since 09:00?",
  "scope": "env=prod, service=nginx and shop-api",
  "status": "closed",
  "seq": 10,
  "created_at": "2026-10-24T09:00:00.000000Z",
  "updated_at": "2026-10-25T08:15:00.000000Z",
  "retain_until": "2026-12-31T00:00:00.000000Z",
  "retain_ceiling": "2026-12-31T00:00:00.000000Z",
  "created_by": { "prefix": "q7Rk2LmP", "name": "claude-agent" },
  "updated_by": { "prefix": "Op5mW2cE", "name": "alice-ops" }
}
```

Evidence that was already removed stays removed.

**Errors**

| Status | Code | When |
|--------|------|------|
| 400 | `bad_request` | The id is not 32 lowercase hex digits, `retain_until` is not an RFC 3339 UTC instant, is in the past, or is before the current `retain_until`, or the body does not read as the request |
| 403 | `forbidden` | The key lacks `server_manage` or one of the write permissions |
| 404 | `not_found` | No investigation has this id |
| 409 | `sequence_conflict` | The investigation's `seq` is no longer `expected_seq` |
| 503 | `service_unavailable` | The metadata store did not answer |

### Delete an investigation

`DELETE /api/v1/investigations/{id}`

Authority: Manage

Deletes the investigation, its log, and its evidence files. Answers 204 with no body. trawld refuses while work attached to it is still running, so that every step has an outcome first. The deleted investigation's results stop being evidence: other keys' reads of them answer 404, and the reads of the key that captured them answer 410 `result_gone` until the result's tombstone ages out.

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" -X DELETE \
  "$TRAWL_URL/api/v1/investigations/6d0c4f7e2a9b41c38e5f0a1b2c3d4e5f"
```

**Errors**

| Status | Code | When |
|--------|------|------|
| 400 | `bad_request` | The id is not 32 lowercase hex digits |
| 403 | `forbidden` | The key lacks `server_manage` or one of the write permissions |
| 404 | `not_found` | No investigation has this id |
| 409 | `bad_request` | A step's work is still running. Delete again once it has recorded its outcome |
| 503 | `service_unavailable` | The metadata store did not answer |

### Attach a capture to an investigation

`POST /api/v1/results` with `investigation`

Authority: Write

A [capture](#capture-a-result) whose body names an investigation runs for that investigation. trawld records it as a step, and keeps its result as the investigation's evidence from the moment it is committed.

**Parameters**

The parameters of [capture a result](#capture-a-result), and:

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `investigation.id` | body | string | yes | The investigation id |
| `investigation.seen_seq` | body | integer | no | The latest `seq` the caller has seen. With it, the response carries `activity` |
| `investigation.purpose` | body | string | no | Why the caller runs the search, at most 1024 bytes. The step records it |

`investigation` refuses a member it does not know.

**Request**

```json
{
  "query": "service=shop-api level=error",
  "window": { "last": "1h" },
  "investigation": {
    "id": "6d0c4f7e2a9b41c38e5f0a1b2c3d4e5f",
    "seen_seq": 3,
    "purpose": "Check whether shop-api logs errors after the deploy"
  }
}
```

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" -H "Content-Type: application/json" \
  -d '{"query": "service=shop-api level=error", "window": {"last": "1h"}, "investigation": {"id": "6d0c4f7e2a9b41c38e5f0a1b2c3d4e5f", "seen_seq": 3, "purpose": "Check whether shop-api logs errors after the deploy"}}' \
  "$TRAWL_URL/api/v1/results"
```

**Response**

```json
{
  "result": {
    "id": "a1b2c3d4e5f60718293a4b5c6d7e8f90",
    "query": "service=shop-api level=error",
    "executed_query": "earliest=\"2026-10-24T08:20:02.000000Z\" latest=\"2026-10-24T09:20:02.000000Z\" service=shop-api level=error\n| sort -_time, -_ingested",
    "source": "live",
    "shape": "events",
    "earliest": "2026-10-24T08:20:02.000000Z",
    "latest": "2026-10-24T09:20:02.000000Z",
    "created_at": "2026-10-24T09:20:02.329114Z",
    "state": "kept",
    "columns": [
      { "name": "_time", "type": "timestamp", "stored_as": "native" },
      { "name": "_ingested", "type": "timestamp", "stored_as": "native" },
      { "name": "service", "type": "string", "stored_as": "native" },
      { "name": "message", "type": "string", "stored_as": "native" },
      { "name": "_raw", "type": "string", "stored_as": "native" }
    ],
    "captured_rows": 12,
    "total_rows": 12,
    "wire_bytes": 4310,
    "file_bytes": 6144,
    "truncated": false,
    "column_facts": [
      { "nulls": 0, "distinct": 6, "distinct_capped": true, "min": { "type": "string", "value": "2026-10-24T08:58:41.112003Z" }, "max": { "type": "string", "value": "2026-10-24T09:19:57.004512Z" } },
      { "nulls": 0, "distinct": 6, "distinct_capped": true },
      { "nulls": 0, "distinct": 1, "distinct_capped": false, "values": [{ "value": { "type": "string", "value": "shop-api" }, "count": 12 }] },
      { "nulls": 0, "distinct": 2, "distinct_capped": false, "values": [{ "value": { "type": "string", "value": "db pool exhausted: connection refused" }, "count": 11 }, { "value": { "type": "string", "value": "health check failed" }, "count": 1 }] },
      { "nulls": 0, "distinct": 6, "distinct_capped": true }
    ],
    "format_version": 1,
    "investigation_id": "6d0c4f7e2a9b41c38e5f0a1b2c3d4e5f"
  },
  "execution": { "started_at": "2026-10-24T09:20:02.011205Z", "duration_ms": 318 },
  "activity": {
    "entries": [
      {
        "seq": 4,
        "updated_seq": 4,
        "actor": { "prefix": "Hn4vX9aT", "name": "jakub" },
        "at": "2026-10-24T09:12:31.000000Z",
        "payload": { "kind": "note", "text": "Deploy 4812 of shop-api went out at 08:58." }
      }
    ],
    "through_seq": 6,
    "more": false
  },
  "step_seq": 5
}
```

The result has `state: "kept"`, no `expires_at`, and `investigation_id`. `step_seq` is the `seq` of the step this capture appended. A client that tracks what it has seen can set its own cursor from it. `activity` is present when the request sent `seen_seq`, and has the meaning it has on an [append](#append-a-keep-a-finding-or-a-note). A capture that fails carries none.

An attached capture runs these checks in this order, on top of the capture's own:

1. **Authority.** The route needs `query`, and trawld checks it before it reads the body. A capture that names an investigation also needs Write authority. trawld checks it once it has read the body, before it checks the query text or the purpose, and a key without it gets 403. A body that does not read as a capture gets 400 `bad_request` before that check.
2. **Purpose and bounds.** The `purpose` bound, then the capture's [bounds](#bounds). A refusal here writes no step, because nothing would run.
3. **The step.** trawld appends a `step` entry with the executed text, the bounds, the request id, the query id, and the purpose, and no outcome. An unknown investigation is 404 and a closed one 409 `investigation_closed`. The append waits for a database connection, for the locks it takes, and for each answer from the database no later than the capture's deadline. When the deadline passes first, the capture gets 429 `capture_busy` with `Retry-After: 1`, and no step is written.
4. **Admission.** The capture is admitted as any capture is, but against the evidence budgets instead of the temporary quota. trawld reserves 8 MiB, `[capture] max_file_bytes`, against the investigation's budget, 64 MiB, and the installation's, 1 GiB, before it takes an executor permit or scans anything. A budget that cannot take the reservation is refused with 409 `evidence_budget_exceeded`. That is not a 429, because evidence frees only as it ages out or when an operator deletes an investigation, so a retry soon after would fail again. The message gives the bytes kept and the budget, and nothing else. When the result commits, the charge becomes the file's real size.
5. **The outcome.** However the work ends, trawld writes the step's outcome once: `success` with the kept result, `refused` with the code of an admission refusal, `timeout`, `failed` with the error's code, or `aborted` when the client went away before the result was committed. The write does not depend on the request staying open. trawld writes the outcomes that no request waits for, such as `aborted`, one at a time on at most one database connection. Each attempt waits no longer than `[capture] deadline_secs` for a connection, for its `BEGIN`, for each statement, for the lift of the statement timeout and for its rollback. An attempt that runs out is cut short or refused at that bound, and its database connection is closed when an answer from the database is still owed. The attempt is then tried again after the others, so the outcome lands once the lock it waits on is released. One wait has no bound: a `COMMIT` already sent is awaited until the database answers, since cutting it short would leave the outcome unknown, and the outcomes queued behind it wait with it. A step left running when trawld stops is marked `interrupted` at the next start.

A success commits the kept result, the step's outcome, and the investigation's next `seq` in one transaction. Writing the outcome advances `seq` again, so the step's `updated_seq` is later than its `seq`. A refusal after step 3 leaves the step with its `refused` outcome.

**Errors**

Every error of [capture a result](#capture-a-result), and:

| Status | Code | When |
|--------|------|------|
| 400 | `bad_request` | `investigation.id` is not 32 lowercase hex digits, `purpose` is over 1024 bytes or holds a NUL character, or `investigation` has an unknown member |
| 403 | `forbidden` | The key lacks `query`, `investigation_read`, or `investigation_write` |
| 404 | `not_found` | No investigation has this id |
| 409 | `investigation_closed` | The investigation is closed |
| 409 | `evidence_budget_exceeded` | The investigation's or the installation's evidence budget cannot take the 8 MiB reservation |
| 429 | `capture_busy` | The capture's deadline passed while its step waited for a database connection, a lock, or an answer from the database. Sent with `Retry-After: 1` |

### Kept evidence

A kept result is a [captured result](#captured-results) with `state: "kept"` and an `investigation_id`. It has no `expires_at`: it lives until its investigation's `retain_until`. Its file is the capture's own file, in the [results root](/reference/configuration/#the-results-root). Keeping never copies it. The evidence budgets count each kept result's file bytes once, however many keeps and findings cite it.

A key with `query` and `investigation_read` reads every investigation's kept results through [read a page](#read-a-page-of-a-captured-result) and [read one cell](#read-one-cell-of-a-captured-result), whoever captured them. Another key's temporary result stays as absent to it as to any key, with the same 404 as an id that does not exist.

When `retain_until` passes, the result's state becomes `pruned` and its file is removed. A read then answers 410 `result_gone` with the message `the investigation's evidence retention ended, so this result's file was removed; the findings that cite it remain`. That holds for the hourly sweep, for the sweep at start, for a write past the deadline, and for a read that finds a kept result past its deadline, which removes the investigation's evidence on the spot. A removed evidence result answers 410 for as long as its investigation exists: its tombstone does not age out to 404, because a finding still cites it.

Kept results survive a restart. They are not listed by [list captured results](#list-captured-results), which lists the key's temporary captures only, and they do not count against the [capture quotas](#admission-and-quotas).

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
data: {"columns":[{"name":"_time","type":"timestamp"},{"name":"message","type":"string"},{"name":"service","type":"string"},{"name":"status","type":"integer"}],"rows":[["2026-10-22T12:00:00.000000Z","GET /","nginx","200"]]}

event: lagged
data: {"missed": 40}
```

| Event | Data | When |
|-------|------|------|
| `data` | A result with one row, `{"columns": [...], "rows": [[...]]}` | Each matching event, for a pipeline with no aggregation |
| `snapshot` | A result with `capped` beside it, `{"columns": [...], "rows": [...], "capped": [...]}` | For an aggregating pipeline, every 500 ms or every 100 matching events, whichever comes first, and only when new events arrived. `capped` is present only when the aggregation dropped input. |
| `lagged` | `{"missed": n}` | The subscriber fell behind the event bus and `n` ingest batches were skipped. A batch holds one or more events, so the number of missed events is unknown. |

Both result frames use the [result column](#result-columns) encoding. A `data` frame's columns are the event's fields in byte order of their names, so `_time` comes before `host`. Each frame carries its own columns, because events differ in their fields. `_time` and `_ingested` are `timestamp`. A field the catalog pins `BIGINT`, `DOUBLE`, `BOOLEAN`, or `TIMESTAMP` is converted to that type the way a pinned `| where` reads it, so a pinned `BIGINT` field arrives as an `integer` decimal string. Any other field is typed by its own value. In a `timechart` snapshot, the `_time` bucket is a `timestamp`.

A `data` or `snapshot` frame that does not decode under its column types is a parse error in the client. If trawld cannot encode a frame, it drops the frame and logs `stream_frame_unencodable`.

A `data` frame:

```json
{"columns":[{"name":"_time","type":"timestamp"},{"name":"message","type":"string"},{"name":"service","type":"string"},{"name":"status","type":"integer"}],"rows":[["2026-10-22T12:00:00.000000Z","GET /","nginx","200"]]}
```

Each `snapshot` replaces the whole result. A live aggregation stops taking
input at three caps, and a snapshot's `capped` lists each cap that dropped
input since the stream started, with its limit:

```json
{"columns":[{"name":"host","type":"string"},{"name":"count","type":"integer"}],"rows":[["web-01","12"]],"capped":[{"cap":"groups","limit":10000}]}
```

| `cap` | Limit | Input it drops |
|-------|-------|----------------|
| `groups` | 10,000 | A new group when the aggregation already holds the limit. A `timechart` counts groups per bucket. |
| `distinct_values` | 10,000 | An unseen value for a `dc()` or `values()` that already holds the limit in one group |
| `exact_values` | 100,000 | A number for a `median` or percentile that already holds the limit in one group |

Reaching a cap exactly drops nothing, so `capped` stays absent. Once a cap
drops input, every later snapshot of the stream reports it, because the
dropped input does not come back. A new stream starts with nothing dropped.
`top`, `rare` and the number of `timechart` buckets have no cap. The batch
lane runs the same aggregation for an `extract kv` pipeline and has no field
to report a partial answer, so it refuses an aggregation that would drop
input with `400 result_too_large` and a message that names the cap and its
limit.

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
  -d '[{"timestamp": "2026-09-11T08:00:00Z", "service": "myapp", "level": "info", "message": "started"}, {"message": "no service here"}]' \
  "$TRAWL_URL/api/v1/ingest"
```

**Response**

```json
{ "accepted": 1, "rejected": 1, "errors": [{ "index": 1, "reason": "missing_service", "message": "missing 'service' field" }] }
```

| Field | Type | Description |
|-------|------|-------------|
| `accepted` | integer | Events written to the WAL |
| `rejected` | integer | Number of error entries, one per validation rejection. Omitted when `0`. |
| `errors` | array | One entry per rejected event. Omitted when empty. |
| `errors[].index` | integer | The zero-based array position, or the zero-based line number counting blank lines |
| `errors[].reason` | string | The reason code from the [rejection table](/reference/events/#rejection-reasons), such as `missing_service` or `invalid_json`. The same value labels `trawl_ingest_events_rejected_total` |
| `errors[].message` | string | What was wrong with the event. It can quote the event's values, such as the env that is not allowed |

A rejected event does not stop its siblings. Repairs do not change `accepted` or `rejected`. A body whose first non-blank byte is not `[` is read as newline-delimited JSON, so a single event object is accepted and a line that is not an event object counts as one rejected event with the response still 200. A sender that reads only the status code, such as Vector, does not see these rejections. [Ingest events rejected](/operate/operational-alerts/#ingest-events-rejected) alerts on them. See [connect and verify a sender](/operate/ingestion/) for an end-to-end check.

trawld writes one WAL file per environment and service group. If any group's
WAL write fails, including its directory fsync, the whole request answers
500 `internal_error` and the body carries no filesystem detail. Groups that
wrote before or after the failed one are still accepted and published, so a
retry of the whole request duplicates them. The events of each failed group
are counted in `trawl_ingest_events_rejected_total{reason="wal_failure"}`.
A persistence rejection does not establish permanent loss; the sender may
retry.

Before it writes anything, trawld reserves hot-buffer space for the whole
request across all its groups. HTTP senders may fill 15/16 of each
`[ingest] hot_buffer_max_*` cap, counted in events and in serialized ndjson
bytes. A request that is larger than that share answers 413
`ingest_batch_too_large` with no `Retry-After`, even when the buffer is empty.
A request that fits the share but not the free space answers 503
`hot_buffer_full` with `Retry-After` set to `[ingest] compaction_interval_secs`.
Space frees only when compaction drains, so a shorter retry only adds refused
work. A refused request writes nothing, so a retry cannot duplicate its
events. Its valid events are counted in
`trawl_ingest_events_rejected_total` under the same reason as the code. A
request refused before parsing counts no events.

When the buffer has no free space at all, trawld refuses the request before
it decompresses or parses the body, so it cannot know the request's size. An
oversized request then answers 503 `hot_buffer_full`, and 413 only once
space frees. A parsed request gets the same answer each time: 413 if it is
oversized, whatever the occupancy, and otherwise 503 until it fits.

A `hot_buffer_max_events` or `hot_buffer_max_bytes` of 0 or 1 leaves HTTP a
share of 0. Every request then answers 413 `ingest_batch_too_large` before
its body is read. The message gives the limit but no request size, and no
events are counted.

**Errors**

| Status | Code | When |
|--------|------|------|
| 400 | `ingest_error` | Empty body, invalid UTF-8, an unparseable or empty JSON array, or a gzip body that fails to decode |
| 413 | `request_too_large` | The body exceeds `[ingest] max_body_bytes`. The message names the setting and its value |
| 413 | `ingest_error` | A gzip body decodes to more than `[ingest] max_body_bytes`. No `Retry-After`. Send smaller batches |
| 413 | `ingest_batch_too_large` | The request holds more than 15/16 of `hot_buffer_max_events` or `hot_buffer_max_bytes`, or a cap of 0 or 1 admits no request. No `Retry-After`. Split the batch |
| 429 | `rate_limited` | The key's ingest bucket is empty |
| 500 | `internal_error` | A group's WAL write failed. Other groups in the request may still have been accepted. |
| 503 | `hot_buffer_full` | The hot buffer has no room for the request. `Retry-After` gives `[ingest] compaction_interval_secs` in seconds. Nothing was written |

### Preview ingest

`POST /api/v1/ingest/preview`

Permission: `server_manage`

Runs a sample through the same parse and canonicalization step as [`/api/v1/ingest`](#ingest-events) and reports what each event would become. Nothing from the sample is stored, counted, or logged. The route covers the HTTP producer only. It exists only when `[ingest] enabled` is `true`. On a node with ingest disabled the server answers 404 with an empty body. It spends the interactive rate bucket and has the interactive body limit, `[server] max_request_body_bytes`. Every response from the route carries `Cache-Control: no-store`, refusals included. Refusals come in this order. While trawld is at its request limit, 503 `request_limit_reached` comes before all of the others. Then come authentication (401, or 503 when the key store is unavailable), a key with no trawl permission (403), the rate limit (429) and a key without `server_manage` (403). trawld reads none of the body before these checks pass and does not wait for it. A client that is still sending the sample when one of these refusals comes can see the connection close or reset instead of the response. Then comes the body limit: a `Content-Length` over it gets 413, and so does a body sent without `Content-Length` that grows past it. Last, the route checks `Content-Encoding` (415), the query string (400) and the sample itself. See [ingest preview](/operate/ingestion/#ingest-preview) for what the preview does not promise.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| body | body | array or ndjson | yes | The same body as `/api/v1/ingest`, at most 500 events. A non-blank line that is not valid JSON counts toward the 500. A blank line does not |
| `peer_ip` | query | string | no | The address trawld would see for the sender, IPv4 or IPv6. An IPv4-mapped IPv6 address, such as `::ffff:10.0.0.7`, is read as its IPv4 address, as real ingest reads it. Default `192.0.2.1`, an RFC 5737 documentation address. trawld classifies either address against `[ingest] trusted_relays` |
| `Content-Encoding` | header | string | no | Absent or `identity`, in any letter case. Any other value answers 415, a list such as `identity, gzip` included |

**Request**

```bash
printf '%s\n' \
  '{"Level":"warn","message":"disk 91% full","service":"myapp","timestamp":"2026-10-01T08:00:00Z"}' \
  '{"message":"no service here"}' |
  curl --fail-with-body --config "$TRAWL_CURL_CONFIG" -H "Content-Type: application/x-ndjson" \
    --data-binary @- "$TRAWL_URL/api/v1/ingest/preview"
```

**Response**

```json
{
  "producer": "http",
  "peer": {
    "ip": "192.0.2.1",
    "given": false,
    "trusted_relay": false
  },
  "arrival": "2026-10-02T23:14:01.881449Z",
  "derivation": {
    "time_from": [
      "_time",
      "timestamp",
      "@timestamp"
    ],
    "severity_from": [
      {
        "field": "severity",
        "dialect": "otel"
      },
      {
        "field": "severity_text",
        "dialect": "otel"
      },
      {
        "field": "level",
        "dialect": "otel"
      }
    ]
  },
  "accepted": 1,
  "rejected": 1,
  "events": [
    {
      "outcome": "accepted",
      "index": 0,
      "input": {
        "Level": "warn",
        "message": "disk 91% full",
        "service": "myapp",
        "timestamp": "2026-10-01T08:00:00Z"
      },
      "event": {
        "_ingested": "2026-10-02T23:14:01.881449Z",
        "_producer": "http",
        "_raw": "{\"Level\":\"warn\",\"message\":\"disk 91% full\",\"service\":\"myapp\",\"timestamp\":\"2026-10-01T08:00:00Z\"}",
        "_repairs": "field.name_case_folded,env.defaulted,host.from_peer",
        "_severity": 13,
        "_time": "2026-10-01T08:00:00.000000Z",
        "env": "prod",
        "host": "192.0.2.1",
        "level": "warn",
        "message": "disk 91% full",
        "service": "myapp",
        "timestamp": "2026-10-01T08:00:00Z"
      },
      "repairs": [
        "field.name_case_folded",
        "env.defaulted",
        "host.from_peer"
      ],
      "lineage": {
        "time": {
          "from": "field",
          "field": "timestamp"
        },
        "severity": {
          "from": "field",
          "field": "level",
          "skipped_unmappable": []
        },
        "fields": [
          {
            "field": "Level",
            "change": "renamed",
            "to": "level",
            "code": "field.name_case_folded"
          }
        ]
      },
      "host_depends_on_sender": true
    },
    {
      "outcome": "rejected",
      "index": 1,
      "input": {
        "message": "no service here"
      },
      "reason": "missing_service",
      "message": "missing 'service' field",
      "host_depends_on_sender": true
    }
  ]
}
```

| Field | Type | Description |
|-------|------|-------------|
| `producer` | string | Always `http` |
| `peer.ip` | string | The peer address used: `peer_ip` as trawld reads it, or `192.0.2.1` |
| `peer.given` | boolean | `true` when the request named `peer_ip` |
| `peer.trusted_relay` | boolean | `true` when the address is in `[ingest] trusted_relays`. A missing `host` is then a rejection, not a fill |
| `arrival` | string | The one arrival time for the whole sample, RFC 3339 UTC with microseconds. It is `_ingested`, and it fills a missing `_time` |
| `derivation.time_from` | array | The fields read for `_time`, in order. The first present one decides |
| `derivation.severity_from` | array | The fields read for `_severity`, in order, each with its `dialect`. The first mappable one decides |
| `accepted` | integer | Accepted events |
| `rejected` | integer | Rejected events, invalid JSON included |
| `events` | array | One entry per non-blank input position, in input order. Empty for a body of only whitespace |

Each entry in `events` has `outcome` set to `accepted` or `rejected`.

| Field | Outcome | Type | Description |
|-------|---------|------|-------------|
| `index` | both | integer | The position, by the same rule as `errors[].index` on [ingest](#ingest-events) |
| `input` | both | any | The parsed input. Absent when the position was not valid JSON. Present for a non-object element, `null` included |
| `event` | `accepted` | object | The canonical event as trawld would store it, `_ingested` included |
| `repairs` | `accepted` | array | Repair codes in the order trawld applied them, the same codes as `_repairs`. See [repair codes](/reference/events/#repair-codes) |
| `lineage.time` | `accepted` | object | `{"from": "field", "field": F}` when field `F` gave `_time`. `{"from": "arrival"}` when the arrival time filled it, with `unparseable` naming the first present source when that source did not parse |
| `lineage.severity` | `accepted` | object | `{"from": "field", "field": F, "skipped_unmappable": [...]}` when `F` gave `_severity`, listing earlier present sources that did not map. `{"from": "missing"}` when no source was present. `{"from": "unmapped", "sources": [...]}` when sources were present and none mapped |
| `lineage.fields` | `accepted` | array | One entry per field change, in the order trawld made them: `field` as that step saw it, `change` (`renamed`, `dropped`, `truncated`, or `stringified`), `to` for a rename, and `code` for a change that is a repair. A stringified value carries no code |
| `reason` | `rejected` | string | The reason code, as on [ingest](#ingest-events) |
| `message` | `rejected` | string | The message real ingest gives for the same event |
| `host_depends_on_sender` | both | boolean | `true` when the request named no peer and the event has no `host` once its field names are normalized. A `Host` or `_host` field counts as `host`. A `host` of `null` counts as no `host`. The stored `host`, or a rejection behind a trusted relay, then depends on the real sender's address. A rejected event can be `true` too, because trawld settles the host before it checks `service` and `env`. Always `false` for a position that is not valid JSON or not an object |

**Errors**

| Status | Code | When |
|--------|------|------|
| 400 | `bad_request` | An unknown query parameter, or a `peer_ip` that is not an IP address. The message does not quote the value |
| 400 | `ingest_error` | Empty body, invalid UTF-8, or an unparseable or empty JSON array. The same refusals as `/api/v1/ingest`. A body of only whitespace is not refused. It answers 200 with no events |
| 403 | `forbidden` | The key lacks `server_manage` |
| 404 | none | `[ingest] enabled` is `false` on this node. The body is empty |
| 413 | `request_too_large` | The body exceeds `[server] max_request_body_bytes`. The message names the setting and its value. Carries `Cache-Control: no-store` |
| 413 | `preview_too_large` | The sample holds more than 500 events. Nothing is truncated. Send a smaller sample |
| 415 | `unsupported_encoding` | The body carries a `Content-Encoding` other than `identity`, `gzip` included |
| 429 | `rate_limited` | The key's interactive bucket is empty |

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
| 503 | `query_resource_exhausted` | The sample went over the spill or memory budget. See [query resource exhausted](#query-resource-exhausted). |
| 503 | `service_unavailable` | The deadline passed before the sample started |
| 503 | `corpus_recovering` | The corpus is unsettled and the values are not cached. See [corpus recovering](#corpus-recovering). |
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

Changes one field's pinned type across the stored corpus, resurrecting shelved values from `_raw` where the new type can read them. One job runs at a time, install-wide. trawld answers within `[server] timeout_secs` of the moment the handler starts the repin, a bound that covers waiting behind another repin request that has not yet claimed its job, the claim, and the rest of the decision: the wait for the corpus gate, the scan, the field-liveness read, the plan write, the pre-flight and the job read. The bound starts after authentication and after the JSON body is read. Authentication, including the update of the key's `last_used` row, and reading the body lie outside it. A job claimed whose decision is still pending at that bound answers 202 with the job as the claim wrote it, dry runs included, and the plan, refusal or outcome is then read from [repin status](#repin-status) by the job's `id`. A request whose claim has not landed by that bound answers 503 `service_unavailable` and starts nothing: a claim that lands later ends `blocked` before its scan, with the corpus unchanged. Disconnecting does not cancel a claimed job. Ingest keeps running during the rewrite, so a job that started with 202 can still end `refused_needs_force` when the finished rewrite exceeds what the request accepted. See [repin a field](/operate/catalog/#repin-a-field) and [recover a lost response](/operate/catalog/#recover-a-lost-response).

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
| 202 | none | The scan and preflight decided within `[server] timeout_secs` and the rewrite started. The body is the job with its recorded plan: the scan counts and `requires_force`. Follow the rewrite through [repin status](#repin-status) with its `id`. |
| 202 | none | The claimed job's decision was still pending at `[server] timeout_secs`. The decision waits on the corpus gate, the scan, the field-liveness read, the plan write and the job read, and any of them can outlast the bound; the scan is the usual cause. The body is the job as the claim wrote it, so it carries no plan, though the plan may already be recorded when you read it. Read [repin status](#repin-status) with its `id` for the plan, the refusal or the outcome. |
| 409 | none | The body is a job with status `refused_needs_force`: the scan found values the new type cannot read, or ambiguous numerals under the `otel` reading, and `force` was not set, or the finished rewrite exceeded an accepted ceiling. `requires_force_reason` names which. |
| 409 | `bad_request` | A repin job is already running |
| 400 | `bad_request` | `field` is an envelope field or not pinned, `to` is not a catalog type, `to` equals the current pin without `force`, `dialect` is unknown or given with a target other than `SEVERITY`, a ceiling is given without `force`, the pin changed between lookup and claim, the staging directory is on a different filesystem from the data root, or free disk is below the affected bytes plus the retention floor |
| 503 | `service_unavailable` | This node has ingest disabled and does not own the data root, the job could not claim the corpus before its scan and ended `blocked`, or no job was claimed within `[server] timeout_secs`. In the last case nothing runs from the request, and it can be retried. |
| 503 | `corpus_recovering` | The corpus is unsettled. The job ends `blocked`, and its `error` names the reason. See [corpus recovering](#corpus-recovering). |

### Repin status

`GET /api/v1/schema/repin/status`

Permission: `schema_read`. With `id`, `schema_read` for any job, or `schema_write` for a job the key requested.

Returns the running job, or else the newest job of any status, so a newer job hides an older one. With `id`, returns that job whatever has run since. A caller follows a [repin](#repin-a-field) that answered 202 this way, and the key that started the job needs no more than the `schema_write` the repin required. A key that holds `schema_write` without `schema_read` reads only the jobs it requested. Every other `id` answers the same 404 response as an `id` no job has. Job ids are sequential, so the existence of an id is not a secret. The rule protects the contents of the job rows, which carry event value samples and name other operators. Served on every node, including those with ingest disabled.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `id` | query | integer | no | A job id, as a repin answered it |

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/schema/repin/status"
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/schema/repin/status?id=7"
```

**Response**

```json
{ "job": { "id": 7, "field": "status", "status": "running", "files_total": 12, "files_done": 4 } }
```

Without `id`, `job` is `null` when no repin has ever run. The job shape is the one [repin a field](#repin-a-field) returns.

**Errors**

| Status | Code | When |
|--------|------|------|
| 400 | `bad_request` | `id` is not an integer |
| 403 | `forbidden` | Without `id`, the key lacks `schema_read`. With `id`, it lacks both `schema_read` and `schema_write`. |
| 404 | `not_found` | No job has that `id`, or the key holds `schema_write` without `schema_read` and did not request the job |
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
| `recent[].error` | string or null | Why the query failed. A query whose request ended before it recorded an outcome, because the client disconnected, says `abandoned: the request ended before the query recorded an outcome`. |
| `retained` | array | Work that still holds a pool permit after its request answered. Disjoint from `active`. |
| `retained[].kind` | string | `query`, `from_saved`, `export`, `scheduled`, `sample`, or `capture` |
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

Lists the key's own completed queries, ordered by execution time descending,
then history ID descending for tied timestamps. An optional filter searches
the stored query text across the key's complete retained history before paging.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `limit` | query | integer | no | Default `100`, maximum `1000` |
| `offset` | query | integer | no | Default `0` |
| `filter` | query | string | no | Literal substring of query text; absent or empty selects all rows. Maximum 32768 decoded UTF-8 bytes. |

Matching uses PostgreSQL `lower()` on both text and filter, with the database's
locale rules. Whitespace remains significant. `%`, `_`, quotes, and backslashes
are literal characters, not wildcard or query syntax. Send values with normal
URL form encoding (`+` decodes to a space).

The raw query payload is limited to 98432 bytes and 64 nonempty
ampersand-separated pairs. The separating `?` is outside this payload; an
additional literal `?` is parameter-name data. Parameter names and the filter
must have valid percent escapes and UTF-8, with no decoded NUL. Repeated decoded
`filter` names are rejected. Unknown parameter values are ignored without
decoding. Existing numeric admission for `limit` and `offset` still applies.
These application bounds do not enlarge HTTP transport limits; the current
URI parser has a 65534-byte limit for the complete URI.

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
| `total` | integer | Matching rows for this key before `limit` and `offset` |

Count and entries use one read-only repeatable-read transaction, so they
describe the same snapshot for this response. A later request can see new or
deleted history. A page beyond the matches returns empty `entries` and keeps
the matching `total`.

**Errors**

| Status | Code | When |
|--------|------|------|
| 400 | `bad_request` | Invalid or oversized history parameters |
| 403 | `forbidden` | The key lacks `query`; permission is checked before parameter parsing |
| 503 | `service_unavailable` | The app-state store did not answer |

### Clear query history

`DELETE /api/v1/history`

Permission: `query`

Deletes all history rows for the key, including rows outside the current page
or filter. Saved queries and other keys' history remain. Queries completed
concurrently can add new history after deletion.

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
| `name` | body | string | yes | Display name; outer whitespace is trimmed, blank names, control characters, and unsafe invisible formatting characters are rejected. Interior spacing, ordinary Unicode, and punctuation are preserved. Unique for the key, case-sensitive |
| `query` | body | string | yes | The DSL query |

Names follow the field-name display-safety rules. Bidi controls, zero-width
characters and joiners, BOM, and soft hyphen are rejected. This also excludes
ZWJ emoji sequences and text that requires ZWJ or ZWNJ shaping. Combining
accents, standalone emoji, skin-tone modifiers, flags, and variation selectors
are allowed.

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
| 400 | `bad_request` | `name` is blank or contains control or unsafe invisible formatting characters |
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

Runs the schedule's next window now, as if its next fire came early. Any schedule can be run this way, in any mode, including a disabled one. The server reads its clock once it holds the claim; call that instant `t`.

- A `since_last` schedule covers `[covered_through, t - lag)`, with the same `max_catchup_intervals` clamp and `window_truncated` flag as a scheduled run. With no `covered_through`, it covers one interval ending at `t - lag`.
- A fixed window covers the span ending at `t - lag`.
- Query mode runs the saved text as written.

The run counts against `max_runs` and records `origin: "manual"`. A successful run advances `covered_through` like a scheduled `since_last` run. It also moves an overdue `next_fire_at` to the first fire after `t`, so the overdue scheduled run does not follow with an older window. A `next_fire_at` already after `t` stays where it is. A failed, timed-out, or interrupted run moves neither. The response returns at once with status `running`, and the run continues in the background.

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" -X POST "$TRAWL_URL/api/v1/saved/7/run"
```

**Response**

```json
{ "id": 43, "query": "earliest=\"2026-09-11T07:55:00.000000Z\" latest=\"2026-09-11T08:25:00.000000Z\" _severity>=error | stats count() by service", "status": "running", "started_at": "2026-09-11T08:30:00+00:00", "window_start": "2026-09-11T07:55:00.000000Z", "window_end": "2026-09-11T08:25:00.000000Z", "window_truncated": false, "window_kind": "since_last", "origin": "manual" }
```

The shape is the [report run](#report-runs) summary. `started_at` is `t`, and the window fields are the bounds the run covers. A query-mode run has no window fields.

**Errors**

| Status | Code | When |
|--------|------|------|
| 400 | `bad_request` | The saved query has no schedule |
| 404 | `not_found` | No saved query with this id belongs to the key |
| 409 | `bad_request` | A run is already in progress, `max_runs` is reached, or a `since_last` window would be empty: `nothing new to read: coverage already reaches 2026-09-11T08:25:00.000000Z, and a run now would end at 2026-09-11T08:24:10.000000Z`. Nothing is recorded. |
| 503 | `corpus_recovering` | The corpus is unsettled. No run is created and the schedule does not move. See [corpus recovering](#corpus-recovering). |

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
| `result_path` | string | Parquet result path relative to the data directory. A zero-row run has one. Absent when the result is stored as a compressed JSON blob: a result with no columns, a `mixed` column, an integer column that needs both negative values and values above the signed 64-bit range, or a failed Parquet write. |
| `window_start`, `window_end` | string | The half-open interval `[start, end)` the run covered |
| `window_truncated` | boolean | `true` when a `since_last` catch-up gap exceeded `[scheduler] max_catchup_intervals` and the start was moved forward |
| `window_kind` | string | `since_last` or `fixed`, the mode the run was claimed under |
| `origin` | string | `scheduled` for a run the scheduler started at a planned fire, `manual` for a run started with `POST /api/v1/saved/{id}/run`. Absent for a run recorded before origins were. |

The four `window_*` fields are absent together for a run without a window: a run of a query-mode schedule.

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

Returns one run with its result rows. trawld reads the Parquet file when `result_path` is set and the stored blob otherwise. A zero-row run returns its typed columns with an empty `rows`.

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/saved/7/runs/42"
```

**Response**

```json
{
  "id": 42, "query": "...", "status": "success", "started_at": "2026-09-11T08:00:00+00:00", "finished_at": "2026-09-11T08:00:01+00:00", "duration_ms": 900, "row_count": 1, "result_path": "scheduled/errors-by-service/run_42.parquet",
  "result": { "columns": [{ "name": "service", "type": "string" }, { "name": "count", "type": "integer" }], "rows": [["nginx", "3"]] }
}
```

`result` uses the [result column](#result-columns) encoding. It is absent for a run with status `running` or `error`.

A run whose window held no events returns its typed columns and no rows:

```json
{
  "id": 43, "query": "...", "status": "success", "started_at": "2026-09-11T09:00:00+00:00", "finished_at": "2026-09-11T09:00:01+00:00", "duration_ms": 850, "row_count": 0, "result_path": "scheduled/errors-by-service/run_43.parquet",
  "result": { "columns": [{ "name": "service", "type": "string" }, { "name": "count", "type": "integer" }], "rows": [] }
}
```

The stored Parquet file keeps the run's types. An `integer` column is stored as `BIGINT`, or as `UBIGINT` when a value is above the signed 64-bit range, and a `timestamp` column as a UTC `TIMESTAMP`.

**Errors**

| Status | Code | When |
|--------|------|------|
| 404 | `not_found` | The run does not exist, belongs to another key, or belongs to a different saved query |

### All runs

`GET /api/v1/runs`

Permission: `saved_query`

Lists the key's runs across every saved query. The server filters by owner, sorts the complete authorized set, then applies pagination.

**Parameters**

| Name | In | Type | Required | Description |
|------|----|------|----------|-------------|
| `limit` | query | integer | no | Default `20` |
| `offset` | query | integer | no | Default `0` |
| `sort` | query | string | no | `net`, `status`, `started`, `duration`, or `rows`. Default `started`. |
| `dir` | query | string | no | `asc` or `desc`. Default `desc`. |

Sort tokens are case-sensitive. The default order is most recent first.

| Sort key | Primary comparison | Tie-breakers |
|----------|--------------------|--------------|
| `net` | PostgreSQL lowercase saved-query name, with C collation | Start time descending, then run ID descending |
| `status` | Canonical status token, lexical C collation | Start time descending, then run ID descending |
| `started` | Stored start timestamp | Run ID in the requested direction |
| `duration` | Numeric duration, with nulls last in both directions | Start time descending, then run ID descending |
| `rows` | Numeric row count, with nulls last in both directions | Start time descending, then run ID descending |

These sort parameters apply only to the global list. Per-net run history keeps its existing order. Separate page requests do not freeze the dataset against new runs or updates.

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
| 400 | `bad_request` | Invalid `sort` or `dir`, duplicate scalar parameters, or malformed `limit` or `offset` |
| 403 | `forbidden` | The key lacks `saved_query`, including when the query parameters are invalid |
| 503 | `service_unavailable` | The app-state store did not answer |

Parameter errors use the normal JSON error envelope. Invalid tokens return `invalid runs sort key` or `invalid runs sort direction`. Extraction failures return `invalid runs parameters` without echoing the submitted values.

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

Returns the latest snapshot the background collector produced. GET and the
[dashboard stream](#dashboard-stream) read the same cache. Neither request
starts a filesystem scan.

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/dashboard"
```

**Response**

```json
{
  "hostname": "trawl.example.com", "listen_addr": "127.0.0.1:5514", "uptime_secs": 86400, "version": "<installed-version>", "healthy": true,
  "pool_capacity": 4, "pool_active": 1, "pool_retained": 0,
  "hot_buffer_events": 120, "hot_buffer_max_events": 100000, "hot_buffer_bytes": 40960, "hot_buffer_max_bytes": 268435456, "hot_buffer_batches": 3,
  "total_queries": 1200, "query_rate": 0.4, "query_errors": 2, "query_timeouts": 0,
  "ingest_events": 500000, "ingest_rate": 12.5, "ingest_rejected": 3,
  "syslog_enabled": true, "syslog_events_udp": 1000, "syslog_events_tcp": 200, "syslog_rate": 0.1, "syslog_parse_errors": 0, "syslog_dropped": 0, "syslog_tcp_connections": 2,
  "wal_files": 1, "wal_bytes": 8192,
  "wal_measurement": { "status": "complete", "sample_age_secs": 2 },
  "last_compaction_secs": 4, "compaction_runs": 8640, "compaction_errors": 0,
  "parquet_files": 12, "parquet_bytes": 5242880,
  "parquet_measurement": { "status": "complete", "sample_age_secs": 5 },
  "capacity": { "headroom": { "measurement": { "status": "not_sampled", "sample_age_secs": null }, "filesystems": [] }, "pressure": { "removals_age": 0, "removals_disk_pressure": 0, "pressure_attempts": 0, "last_sweep": null }, "environments": [], "growth_excluded": [] },
  "sse_active": 0, "sse_max": 32,
  "scheduler_enabled": true, "scheduler_schedules": 1,
  "recent_queries": [], "active_queries": []
}
```

| Field | Type | Description |
|-------|------|-------------|
| `query_rate`, `ingest_rate`, `syslog_rate` | number | Smoothed per-second rates computed by the server. `ingest_rate` covers HTTP only. |
| `query_errors`, `query_timeouts` | integer | Counts over the recent-query window |
| `ingest_events`, `ingest_rejected` | integer | HTTP accepted and rejected event counts since process startup. Excludes syslog. |
| `syslog_enabled` | boolean | Configured syslog enablement. This does not test listener health. |
| `syslog_events_udp`, `syslog_events_tcp` | integer | Messages received by each transport since process startup. Reception does not prove persistence. |
| `syslog_parse_errors`, `syslog_dropped` | integer | Parse errors and dropped events since process startup. Drops include a full listener queue, events refused by hot-buffer admission at shutdown, and single events too large to admit |
| `syslog_tcp_connections` | integer | Current active syslog TCP connections |
| `wal_files`, `wal_bytes` | integer | Last complete count and byte total of WAL `.ndjson` files, including active files. These are not compaction-eligible totals. |
| `parquet_files`, `parquet_bytes` | integer | Last complete count and byte total of ingested Parquet files. Excludes saved report files under `scheduled/` directories. |
| `wal_measurement`, `parquet_measurement` | object | Required measurement metadata for the corresponding totals, as defined below |
| `capacity` | object | Required. Disk headroom, pressure-deletion evidence, and retention reach, as defined in [Capacity](#capacity) |
| `compaction_runs` | integer | Successful compaction cycles since process startup. A cycle can succeed with no eligible work. |
| `compaction_errors` | integer | Error tally since process startup, including failed cycles and loss/error tallies. Can accompany successful cycles and exceed their count. |
| `last_compaction_secs` | integer or null | Seconds since the last successful cycle. `0` is a reported success with zero elapsed seconds. `null` means no successful cycle reported since startup. |
| `recent_queries`, `active_queries` | array | The same entries as `/api/v1/queries` without `own` |

A nonzero historical error tally does not establish a current incident.
`compaction_errors / compaction_runs` is not a failure percentage.

Each storage metadata object has a required `status` and nullable
`sample_age_secs`. The producer emits both keys.

| Status | Meaning | Totals and `sample_age_secs` |
| --- | --- | --- |
| `not_configured` | An optional source is absent, such as no WAL writer | Zero placeholders and `null` age |
| `not_sampled` | A configured source has not completed an attempt | Zero placeholders and `null` age |
| `complete` | The latest completed attempt succeeded | Measured totals and an integer age, including measured zero and age `0` |
| `failed` | The latest completed attempt failed | Last complete totals and their age if a sample exists. Otherwise, zero placeholders and `null` age. |

WAL configuredness comes from actual writer presence. The Parquet archive
remains configured on a query-only server. A missing, non-directory, or
unreadable configured root produces `failed`, even if boot accepts that root's
absence. An absent archive is not a measured empty directory.

WAL and Parquet scans are independent. A complete scan has no unresolved
coverage error under the collector's policy. After a NotFound error, the
collector skips a descendant only if it confirms that the descendant is absent.
Other enumeration, entry inspection, and metadata errors reject the attempt.
The configured root must remain present and enumerable at completion.
Failure preserves the prior complete sample rather than publishing partial
totals. Measurement metadata exposes stable status, never filesystem error messages or paths.

`sample_age_secs` uses monotonic elapsed time since the last successful scan
completed. The server calculates it when it assembles the dashboard snapshot.
A failed attempt does not reset that age. Age has no fixed maximum and is not a
transactional filesystem timestamp. A `failed` measurement can have an age;
an age alone does not imply `complete`.

A live dashboard stream describes the connection, not the disk measurement.
Clients display the snapshot's supplied age without a local age ticker.
A retained snapshot keeps that age unchanged until a new snapshot arrives.

#### Capacity

The `capacity` object reports disk headroom, retention's pressure-deletion
evidence, and each environment's retention reach. [Read disk and
retention](/operate/health/#read-disk-and-retention) explains how to read it.

```json
{
  "headroom": {
    "measurement": { "status": "complete", "sample_age_secs": 3 },
    "filesystems": [
      { "roles": ["data", "spill"], "total_bytes": 500000000000, "available_bytes": 800000000,
        "floor": { "state": "armed", "floor_bytes": 1073741824, "deficit_bytes": 273741824 } },
      { "roles": ["wal"], "total_bytes": 64000000000, "available_bytes": 60000000000, "floor": null }
    ]
  },
  "pressure": {
    "removals_age": 14, "removals_disk_pressure": 3, "pressure_attempts": 2,
    "last_sweep": { "outcome": "completed", "age_secs": 754 }
  },
  "environments": [
    { "env": "archive", "max_age_days": 0, "oldest_date": "2026-06-01", "stored_bytes": 9000000000,
      "reach": { "state": "keep_forever", "observed_first": "2026-09-19", "observed_last": "2026-09-25", "observed_days": 7, "mean_daily_bytes": 120000000 } },
    { "env": "fresh", "max_age_days": 30, "oldest_date": "2026-09-24", "stored_bytes": 40000000,
      "reach": { "state": "withheld", "reason": "insufficient_history" } },
    { "env": "prod", "max_age_days": 90, "oldest_date": "2026-08-10", "stored_bytes": 9000000000,
      "reach": { "state": "projected", "observed_first": "2026-09-19", "observed_last": "2026-09-25", "observed_days": 7,
                 "low": { "kind": "days", "days": 38 }, "high": { "kind": "days", "days": 52 } } }
  ],
  "growth_excluded": ["fresh"]
}
```

| Field | Type | Description |
|-------|------|-------------|
| `headroom.measurement` | object | Status and age of the headroom measurement, as defined for storage above. One measurement reads every role, so all rows share it. |
| `headroom.filesystems` | array | One row per device, from the last complete measurement. Empty until a complete measurement exists. A failed measurement keeps the last complete rows. |
| `headroom.filesystems[].roles` | array | The roles on this device, in the order `data`, `wal`, `spill`. Never empty. |
| `headroom.filesystems[].total_bytes`, `available_bytes` | integer | The device's total and available bytes. Available bytes are never added across devices. |
| `headroom.filesystems[].floor` | object or null | The deletion floor on the row that holds `data`, and `null` on every other row |
| `pressure.removals_age`, `pressure.removals_disk_pressure` | integer | Date directories removed since process start, with a successful delete, by trigger |
| `pressure.pressure_attempts` | integer | Sweeps since process start whose first free-space check found the data filesystem below the floor |
| `pressure.last_sweep` | object or null | `outcome` and `age_secs` of the last finished sweep. `null` before the first sweep. |
| `environments` | array | Environments with at least one date directory that holds Parquet, sorted by `env` |
| `environments[].max_age_days` | integer | The effective age limit. `0` keeps the data forever. |
| `environments[].oldest_date` | string | The oldest date directory that holds Parquet, `YYYY-MM-DD`. An empty date directory does not count. |
| `environments[].stored_bytes` | integer | Parquet bytes in the environment's date directories |
| `environments[].reach` | object | The retention reach, tagged by `state` |
| `growth_excluded` | array | Environments with a finite `max_age_days` and fewer than 3 observed days, sorted. The projection reserves their stored bytes and leaves out their growth. Empty while every reach is withheld for `measurement_unavailable` or `retention_suppressed`. |

`floor.state` is `off` when `min_free_disk_bytes` is 0, and `armed` otherwise.
An `armed` floor carries `floor_bytes` and `deficit_bytes`. `deficit_bytes` is
`floor_bytes - available_bytes` while available bytes are strictly below the
floor, and 0 otherwise.

`last_sweep.outcome` is one of the following. When more than one applies, the
first one listed wins:

| Outcome | Meaning |
| --- | --- |
| `failed` | The sweep returned an error, a deletion failed, or the sweep task panicked |
| `suppressed` | A repin, or publication markers that cannot be read, stopped the sweep |
| `exhausted_below_floor` | Pressure deletion ran out of candidates with available bytes still below the floor |
| `completed` | Every step ran, and every deletion it tried succeeded |

`last_sweep.age_secs` is monotonic seconds since the sweep finished,
calculated when the server assembles the snapshot.

`reach.state` is one of the following:

| State | Fields | Meaning |
| --- | --- | --- |
| `projected` | `observed_first`, `observed_last`, `observed_days`, `low`, `high` | A finite `max_age_days`. `low` uses the largest observed day and `high` uses the mean observed day. |
| `keep_forever` | `observed_first`, `observed_last`, `observed_days`, `mean_daily_bytes` | `max_age_days` is 0. No time claim. |
| `withheld` | `reason` | No projection. The object carries no number. |

`observed_first` and `observed_last` are UTC dates, `YYYY-MM-DD`, and
`observed_days` counts the observed days, from 3 to 7.

`low` and `high` are each tagged by `kind`:

| `kind` | Meaning |
| --- | --- |
| `days` | `days` whole days of the policy, rounded down |
| `full_policy` | The environment keeps its full policy |
| `disk_fills_first` | `min_free_disk_bytes` is 0 and the full policy does not fit. No days are given. |

`reason` is `insufficient_history`, `retention_suppressed`, or
`measurement_unavailable`. See [Act on a withheld
reach](/operate/health/#act-on-a-withheld-reach).

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

### Server capabilities

`GET /api/v1/capabilities`

Permission: any recognized trawl permission

Returns what this server offers the key before it sends a query: the version, the DSL it parses, the bounds a query and a capture run under, the capture quotas and admission numbers, the request rate that applies to the key, the evidence policy of investigations, the origin for links to Search, and the health report. A client such as `trawl mcp` reads it once to plan its requests. trawld answers from its own memory: the numbers are the configuration it started with, and the route reads no database and no data file beyond what [server health](#server-health) probes.

**Request**

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/capabilities"
```

**Response**

The example shortens `dsl.functions`. A real response lists every function.

```json
{
  "version": "<installed-version>",
  "dsl": {
    "stages": ["from", "stats", "timechart", "where", "sort", "limit", "head", "tail", "let", "eval", "extract", "rex", "table", "fields", "top", "rare", "dedup", "drop", "rename", "pivot", "sample", "eventstats"],
    "functions": ["count", "avg", "sum", "min", "max", "dc", "distinct_count"],
    "max_query_bytes": 65536,
    "max_pipeline_stages": 128
  },
  "bounds": {
    "default_window_secs": 900,
    "max_span_secs": 604800,
    "deadline_secs": 15,
    "max_rows": 5000,
    "max_wire_bytes": 4194304,
    "page_max_rows": 200,
    "ttl_secs": 900,
    "query_timeout_secs": 30,
    "max_result_rows": 100000
  },
  "quotas": {
    "per_key_results": 64,
    "per_key_bytes": 67108864,
    "installation_results": 512,
    "installation_bytes": 536870912
  },
  "admission": {
    "per_key_running": 1,
    "installation_running": 2,
    "queue_depth": 4,
    "rate_rpm": 100
  },
  "evidence": {
    "idle_secs": 604800,
    "ceiling_secs": 2592000,
    "per_investigation_bytes": 67108864,
    "installation_bytes": 1073741824
  },
  "link_origin": null,
  "health": {
    "status": "ok",
    "checks": { "duckdb": "ok", "auth_db": "ok", "storage_db": "ok", "data_path": "ok", "ingest_capacity": "ok", "corpus": "ok" },
    "version": "<installed-version>"
  }
}
```

| Field | Type | Description |
|-------|------|-------------|
| `version` | string | The trawld package version |
| `dsl.stages` | array | Every pipe stage keyword the parser accepts, in the grammar's order |
| `dsl.functions` | array | Every function name the parser accepts, aggregates and scalars, in the grammar's order. An alias such as `dc` is listed apart from `distinct_count` |
| `dsl.max_query_bytes` | integer | The longest query text the parser reads, in bytes. A longer text is a `parse_error` |
| `dsl.max_pipeline_stages` | integer | The most pipeline stages one query may carry. More is a `validation_error` |
| `bounds.default_window_secs`, `bounds.max_span_secs`, `bounds.max_rows`, `bounds.max_wire_bytes`, `bounds.page_max_rows`, `bounds.ttl_secs` | integer | The [`[capture]`](/reference/configuration/#capture) keys of the same names. Seconds and bytes as the keys count them |
| `bounds.deadline_secs` | integer | The seconds one capture may take: `[capture] deadline_secs`, capped by `[server] timeout_secs`, as a capture applies it |
| `bounds.query_timeout_secs` | integer | `[server] timeout_secs`. See [query timeout](#query-timeout) |
| `bounds.max_result_rows` | integer | `[server] max_result_rows`: the most rows one query result holds |
| `quotas` | object | `per_key_results`, `per_key_bytes`, `installation_results` and `installation_bytes`, the `[capture]` keys of the same names. See [admission and quotas](#admission-and-quotas) |
| `admission.per_key_running`, `admission.installation_running`, `admission.queue_depth` | integer | The `[capture]` keys of the same names |
| `admission.rate_rpm` | integer or null | The requests per minute this key may send to the interactive routes, this one included: the largest `rate_rpm` among the key's roles, or `[server.rate_limit] default_rpm` when no role sets one. `null` when the default is `0` and no role sets one, so the key is not rate limited. See [rate limiting](#rate-limiting) |
| `evidence` | object | `idle_secs`, `ceiling_secs`, `per_investigation_bytes` and `installation_bytes`: the [`[investigation]`](/reference/configuration/#investigation) keys of the same names with an `evidence_` prefix. Seconds and bytes as the keys count them |
| `link_origin` | string or null | [`[server] link_origin`](/reference/configuration/#server) in its normalized form. `null` when it is not set, and then clients build no link to Search |
| `health` | object | The body [server health](#server-health) answers at the same moment. The route answers 200 even when `health.status` is `unavailable` |

The response does not carry retention reach. Read it from the dashboard snapshot's [capacity](#capacity), which needs `server_manage`.

**Errors**

| Status | Code | When |
|--------|------|------|
| none | | No errors beyond [Authentication](#authentication) and [Rate limiting](#rate-limiting) |

## Metrics

### Prometheus metrics

`GET /metrics`

Permission: none. The route is unauthenticated and outside `/api/v1`.

Renders the Prometheus text exposition format. Each scrape collects process
metrics and trawld gauges first. Storage collection shares the stats emitter's
cached measurements and retry timing.

The following numeric gauges carry the last complete storage totals:

| Gauge | Measurement |
| --- | --- |
| `trawl_wal_files` | WAL `.ndjson` file count, including active files |
| `trawl_wal_bytes` | Byte total for those WAL files |
| `trawl_parquet_files_total` | Ingested Parquet file count, excluding saved report files |
| `trawl_parquet_size_bytes` | Byte total for those ingested Parquet files |

Before the first successful measurement, collection does not publish these
numeric gauges. A complete empty scan publishes zero. A later failed attempt
retains the last complete totals.

These gauges expose neither measurement status nor sample age. A flat gauge
cannot establish collector health. Those facts are available in the
[dashboard measurement metadata](#dashboard-snapshot).

These series describe disk headroom and retention's pressure-deletion
evidence. [Manage retention](/operate/retention/#read-headroom) explains how to
read them.

| Series | Type | Measurement |
| --- | --- | --- |
| `trawl_disk_total_bytes{role}` | gauge | Total bytes of each filesystem trawld writes to. `role` is `data`, `wal`, or `spill`. `spill` is the filesystem of `[server] query_scratch_dir` |
| `trawl_disk_available_bytes{role}` | gauge | Bytes available to trawld on each filesystem it writes to. `role` is `data`, `wal`, or `spill` |
| `trawl_retention_min_free_disk_bytes` | gauge | The deletion floor, `min_free_disk_bytes`, set at startup. `0` means pressure deletion is off |
| `trawl_retention_deletions_total{trigger}` | counter | Date directories that retention removed with a successful delete. `trigger` is `age` or `disk_pressure`. Both series start at zero |
| `trawl_retention_pressure_attempts_total` | counter | Retention sweeps whose first free-space check found the data filesystem below the floor. Starts at zero |

The two disk gauges publish one series per device, labelled with the first
role the device holds in the order `data`, `wal`, `spill`. A device that holds
`data` and `spill` publishes only `role="data"`, so `sum()` over the series
never counts a device twice. trawld groups the roles by device at each
measurement. If a remount merges two devices while trawld runs, the series of
the role that lost its own device keeps its last value until trawld restarts.
Like the storage gauges, the disk gauges appear after the first complete
measurement, and a failed measurement keeps the last complete values.

The counters start at process start. No series carries a path, a device, or an
environment name. No series exports retention reach: reach is derived from
these readings and the stored Parquet, and a Prometheus gauge carries no
measurement status (ADR-0033). Read reach from the
[dashboard capacity object](#capacity).

These series describe [hot-buffer admission](/architecture/data-flow/#hot-buffer-admission).
A node without a hot buffer publishes the refusal counter and the compaction
interval but none of the hot-buffer gauges.

| Series | Type | Measurement |
| --- | --- | --- |
| `trawl_hot_buffer_events`, `trawl_hot_buffer_bytes` | gauge | Events and serialized ndjson bytes resident in the hot buffer |
| `trawl_hot_buffer_max_events`, `trawl_hot_buffer_max_bytes` | gauge | The full caps, `hot_buffer_max_events` and `hot_buffer_max_bytes`. HTTP and syslog may fill 15/16 of each |
| `trawl_hot_buffer_oldest_batch_age_seconds` | gauge | Seconds since the oldest resident batch was inserted. A batch that boot hydration loaded counts from the time in its WAL file name. `0` when the buffer is empty |
| `trawl_hot_buffer_admission_state` | gauge | `0` open, `1` pressure (at or above half of either cap), `2` refusing (a reservation was refused for lack of space). Pressure and refusing clear only below a quarter of both caps |
| `trawl_hot_buffer_admission_refusals_total{producer,kind}` | counter | Refused reservations. `producer` is `http`, `syslog`, or `trawld`. `kind` is `full`, no free space now, or `oversized`, larger than the producer's share and never admissible. All six series start at zero |
| `trawl_compaction_interval_seconds` | gauge | `[ingest] compaction_interval_secs` |
| `trawl_syslog_events_dropped_total{reason}` | counter | Syslog events dropped before a WAL write. `backpressure` means the batcher was holding a group that hot-buffer admission refused. `queue_full` means the listener queue was full while the batcher was not blocked, or the batcher was gone |
| `trawl_hot_buffer_duplicate_batches_total` | counter | Batches dropped because a batch with the same identity was already resident. The resident batch stays. Any increment is a writer bug. Starts at zero |

#### Query engine metrics

These series show the executor pool and the engine sample behind the `duckdb` health check. They carry no label and no principal-derived value. Saturation is visible here and never changes health.

| Series | Type | Measurement |
| --- | --- | --- |
| `trawl_query_permits_in_use` | gauge | Executor permits held now, by running queries, exports, field samples, and retained work |
| `trawl_query_pool_size` | gauge | The executor pool size, `[server] max_concurrent_queries` |
| `trawl_query_permits_retained` | gauge | Permits held by physical work whose request has already ended. A subset of `trawl_query_permits_in_use` |
| `trawl_engine_sample_age_seconds` | gauge | Seconds since the latest probe started, or since the sampler started before the first probe. It resets every 10 seconds while probes finish, and it keeps growing while one is wedged |

`trawl_query_permits_in_use` at `trawl_query_pool_size` means searches queue, not that the engine is down. Alarm on the sample age instead, or on `checks.duckdb` reading `error`. `trawl_health_check{subsystem="duckdb"}` is set each time `/api/v1/health` answers. It reads `1` for `ok` and `slow`, and `0` for `error`. See [diagnose a 503 or 504 from a query](/operate/health/#diagnose-a-503-or-504-from-a-query).

#### Maintenance memory metric

Compaction, rollup, conform and repin each open a DuckDB database bounded by `[ingest] compaction_memory_limit`. See [the memory arithmetic](/reference/configuration/#the-memory-arithmetic).

| Series | Type | Measurement |
| --- | --- | --- |
| `trawl_maintenance_resource_exhausted_total` | counter | Maintenance statements that ran out of `compaction_memory_limit`. trawld logs `maintenance_resource_exhausted` and keeps the inputs. Compaction and rollup retry them on the next pass. A repin statement that runs out with no fallback ends the job `failed`, and the job is not retried. Boot conformance retries on the next boot. Optional sampling falls back and the pass completes, but the statement is still counted: compaction and conform record the conflict without samples, and the repin scan reruns its sampled measurement as a plain count. Starts at zero, carries no label |

#### Restart visibility metrics

These series describe [restart visibility](/architecture/data-flow/#restart-visibility). `trawl_corpus_unsettled` and `trawl_hydration_files_total` start at zero after recorder installation. `trawl_health_check{subsystem="corpus"}` is set each time `/api/v1/health` answers.

| Series | Type | Measurement |
| --- | --- | --- |
| `trawl_corpus_unsettled{reason}` | gauge | `1` while corpus reads refuse for `reason`, else `0`. `reason` is `restart_backlog` or `rollup_pending`. Each series is set on its own, so both can read `1` |
| `trawl_hydration_files_total{outcome}` | counter | WAL files that boot hydration looked at, by outcome. Every outcome except `hydrated` leaves overhang |
| `trawl_health_check{subsystem="corpus"}` | gauge | `1` when `checks.corpus` reads `ok`, else `0` |

| `outcome` | Meaning |
| --- | --- |
| `hydrated` | Loaded into the hot buffer |
| `capacity` | Did not fit in the capacity or the read budget left, so it was skipped |
| `oversized` | Larger than a full cap, so it can never fit |
| `undecodable` | Not exactly what the WAL writer produces, including a file name that is not a writer name. Compaction's decoder takes it |
| `unreadable` | Not a regular file, a symlink, a read that failed, or a file whose length changed during the read |
| `claimed` | In an environment and service that a pending publication marker blocks |
| `unlisted` | A scope that could not be listed: the WAL root, an entry in it, or an environment directory. Counts once per scope, not per file |

Files past the examine bound of `hot_buffer_max_events` + 1 count under no outcome. `boot_hydration` logs `examine_bound_hit=true` for them.

#### App-state pool metric

trawld gives a database connection back to its pool only outside a transaction. Work cancelled while Postgres' answer to its `BEGIN` is on the way, such as a request whose client hangs up at that moment, leaves its session inside a transaction. trawld closes that session when it comes back, and Postgres ends the transaction with it, so no later caller runs inside it.

| Series | Type | Measurement |
| --- | --- | --- |
| `trawl_pool_connections_returned_in_transaction_total{pool}` | counter | Connections that came back to the app-state pool inside a transaction and were closed instead of reused. `pool` is `storage`. trawld logs `pool_connection_returned_in_transaction` for each. Starts at zero |

The fleet keystore pool closes such a session too and logs the same event with `pool=keystore`. It publishes no series.

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

### Operational alert counters

The [operational alert pack](/operate/operational-alerts/) uses the following
counter selections. Units are distinct: events, task failures, operation
attempts, and files must not be added into one loss total. New metric labels
contain no environment, event-service name, path, error text, or request ID.
Prometheus scrape-target labels are separate and remain on every alert.

| Alert | Counter selection | Unit and observation owner |
| --- | --- | --- |
| `TrawlSyslogQueueDiscard` | `trawl_syslog_events_dropped_total{reason}` | Events abandoned by the syslog receive path; `reason` is `backpressure` or `queue_full` |
| `TrawlSyslogWalDiscard` | `trawl_syslog_wal_events_discarded_total` | Events in the failed syslog pipeline WAL group, counted once at group abandonment; no metric labels |
| `TrawlTelemetryCapacityDiscard` | `trawl_telemetry_events_dropped_total{reason=~"preinit_cap\|buffer_cap"}` | Events abandoned at the pre-sink or active-buffer capacity boundary |
| `TrawlSyslogWriteOutcomeUncertain` | `trawl_syslog_write_tasks_failed_total` | Failed syslog flush tasks, counted at the handled `JoinError`; no metric labels |
| `TrawlTelemetryWriteOutcomeUncertain` | `trawl_telemetry_events_dropped_total{reason="write_crashed"}` | Events consumed from the in-memory telemetry batch when its write task fails; WAL bytes may already exist |
| `TrawlHttpPersistenceRejection` | `trawl_ingest_events_rejected_total{reason="wal_failure"}` | Events in failed HTTP WAL groups, counted during final ingest accounting |
| `TrawlIngestEventsRejected` | `trawl_ingest_events_rejected_total{reason!~"wal_failure\|hot_buffer_full\|ingest_batch_too_large"}` | Events that each broke an event-contract rule and were refused alone, under that rule's `reason`; counted during final ingest accounting |
| `TrawlTelemetryWalWriteFailure` | `trawl_telemetry_wal_write_failures_total` | Failed telemetry write attempts, including retained retries and crashed tasks; no metric labels |
| `TrawlWalDurabilityDegraded` | `trawl_wal_durability_failures_total{operation="parent_directory_sync"}` | Failed WAL directory sync operations, each of which rejected its write; counted by the WAL writer |
| `TrawlCompactionOperationFailure` | `trawl_compaction_operation_failures_total{operation}` | Explicit failed attempts, using the eight closed operations below |
| `TrawlFileQuarantine` | `trawl_files_quarantined_total{kind}` | Files successfully renamed into quarantine; `kind` is `wal`, `parquet`, or `rollup_temporary` |
| `TrawlPublicationRecoveryBlocked` | `trawl_publication_recovery_total{outcome=~"contradictory\|failed"}` | Publication markers that recovery left blocking their service, counted once per marker per recovery pass |
| `TrawlIngestAdmissionRefusing` | `trawl_hot_buffer_admission_refusals_total{kind="full"}`, summed over `producer` | Reservations refused for lack of space; `oversized` refusals are excluded |
| `TrawlHotBufferDrainStalled` | `trawl_hot_buffer_oldest_batch_age_seconds` compared with `trawl_compaction_interval_seconds` | Gauges, not counters: seconds since the oldest resident batch was inserted, against ten compaction intervals with a 60-second floor |
| `TrawlCorpusUnsettled` | `trawl_corpus_unsettled`, the maximum over `reason` | A gauge, not a counter: `1` while corpus reads refuse, held for ten minutes. A change of reason does not restart the delay |

The telemetry drop counter's closed `reason` set is `preinit_cap`,
`buffer_cap`, `write_crashed`, and `unmetered_cap`. The capacity and
uncertain-outcome rules select disjoint reasons. No rule selects
`unmetered_cap`: it counts unmetered server-failure events past the fixed
cap of 60 persisted per minute, and each of those events still reaches
stdout. A crashed telemetry write also increments the
inclusive failed-attempt counter; both telemetry failure alerts can fire.
The WAL durability counter has only `operation="parent_directory_sync"`.
`TrawlIngestEventsRejected` selects every rejection reason except
`wal_failure`, `hot_buffer_full`, and `ingest_batch_too_large`, so a new
reason alerts without a rule change. The two rejection rules select disjoint
reasons. `hot_buffer_full` and `ingest_batch_too_large` stay diagnostic:
they refuse the whole request with a 503 or 413 that the sender sees.

All 36 selected counter series are initialized at zero after recorder
installation and before the first scrape, independently of feature enablement.
Every `reason` of `trawl_ingest_events_rejected_total` starts at zero too, so
the first refusal for a reason is an observed increase.
Initialization preserves accumulated values. The exporter has no idle expiry
for these baselines. Counters reset when the process restarts. The two gauges
that `TrawlHotBufferDrainStalled` selects are set at startup and on every
scrape. The age gauge exists only on a node with a hot buffer.
`trawl_corpus_unsettled` starts at zero for both reasons and is read from
the publication gate on every scrape.

The twelve counter rules use a fixed ten-minute `increase` window with no `for` delay,
aggregation, current-value guard, or ingest-enable gate. The window describes
scraped observations, not an exact event-loss count. A first nonzero sample
cannot recover a prior baseline; an unseen process lifetime is unobservable.
Telemetry increments before recorder installation are not recorded at all.
See the runbooks for [sampling and resolution limits](/operate/operational-alerts/#read-the-observation-window).

### Compaction operation labels

| `operation` | Failed attempt and counting boundary |
| --- | --- |
| `wal_root_scan` | Read or enumerate the WAL root; confirmed cold-start absence is excluded |
| `wal_environment_scan` | Read or enumerate one WAL environment |
| `chunk` | Compact one chunk, including a handled blocking-task failure; best-effort failure counts even when the cycle returns success |
| `daily_rollup_scan` | Scan daily-rollup directories; confirmed `NotFound` at directory-scan boundaries is excluded |
| `daily_rollup_unit` | Roll up one daily unit, including a handled task failure |
| `pending_rollup_scan` | Initialize the publication gate by scanning pending markers; a latched failure is counted once, not again on each read refusal |
| `pending_rollup_recovery` | Recover pending rollup markers; the coordinated recovery wrapper owns returned errors, and the caller owns a handled task failure |
| `consumed_wal_removal` | Delete or rename aside a consumed WAL file after publication. On failure, the publication marker stays and blocks the service until recovery retires the file |

Propagating a returned error through callers does not add another failure.
At the WAL and daily-rollup root, one scan attempt counts once even if
several entries cannot be enumerated or inspected. Successfully inspected
environments remain eligible for the existing processing path.
Independent attempts remain separate. A daemon without a hot buffer creates
a fresh publication gate for each cycle, so another failed scan is a new
attempt. A successful quarantine and a later operation failure are separate
facts even when they occur in one cycle.

`CompactionStats.total_errors` retains its existing mixed error/quarantine
meaning. It is not relabelled as failed cycles. Idle work, disabled ingestion,
and intentional repin suppression or waiting do not emit these operation
failures. The `wal_root_scan` and `daily_rollup_scan` directory scans ignore
confirmed missing paths. Pending-rollup recovery also ignores a missing
directory at its initial directory read. Other recorded scans and later
file-read, publication, and recovery failures can count `NotFound`, including
races with retention. The alert
reports the failed attempt, not its cause. Stale
temporary-file cleanup and empty-directory housekeeping are outside this
closed inventory. Retiring a replaced file as `.parquet.merged` is not a
corrupt-file quarantine. These counters do not measure backlog eligibility
or prove that compaction is making progress.

### Publication recovery outcomes

`trawl_publication_recovery_total{outcome}` counts the publication markers
that recovery examined. Recovery runs once at boot, before the daemon
serves, and again at the start of each compaction tick.
`TrawlPublicationRecoveryBlocked` selects the `contradictory` and
`failed` outcomes. All four outcomes are initialized at zero after recorder
installation.

| `outcome` | Meaning |
| --- | --- |
| `published` | The canonical output carries the recorded identity. Recovery retired the consumed WAL files and removed the marker |
| `unpublished` | The output was never renamed into place. Recovery removed the marker and the temporary output, and kept the WAL for the next compaction |
| `contradictory` | The evidence contradicts itself, for example a canonical output with another identity and no temporary output. Recovery touched nothing, and the service stays blocked |
| `failed` | A filesystem error stopped recovery of one marker. While the marker remains, the service stays blocked and the next tick retries. Recovery removes a marker only after its outcome is settled, so an error after that leaves nothing to retry: the rows are already published, or still in the WAL for the next compaction |

Each `contradictory` or `failed` outcome also counts once in
`CompactionStats.total_errors` for that tick. A `contradictory` marker
repeats on every tick until an operator resolves it. The
`publication_recovery_failed` log event for one marker names the env, the
service, the marker path, and the `reason` or `error`. When recovery cannot
list an env's WAL directory, the event carries only the env and the error,
and nothing counts on this metric. A compaction tick lists the WAL root
before recovery runs, and a failure of that listing counts under
`wal_root_scan` and ends the tick before recovery. When that listing
succeeds and recovery's own listing of the root fails, the event carries
only the error and counts one `failed`.
