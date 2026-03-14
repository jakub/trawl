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

API keys are managed with `trawl-admin`:

```bash
trawl-admin key create --role analyst --name "my-key"
trawl-admin key list
trawl-admin key revoke <key-prefix>
```

### Roles

| Role | Permissions |
|------|------------|
| `admin` | Full access including dashboard, key management |
| `analyst` | Query, validate, schema, history, saved queries, export |
| `reader` | Query and validate only |
| `ingest` | Ingest endpoint only |

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
```

Returns column names and types discovered from the data.

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

Returns the identity and permissions of the current token.

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

Accepts JSON arrays or ndjson. Supports optional gzip compression (`Content-Encoding: gzip`). Requires a token with `ingest` role.

### Metrics

```
GET /metrics
```

Prometheus metrics. Unauthenticated. Outside the `/api/v1` namespace.
