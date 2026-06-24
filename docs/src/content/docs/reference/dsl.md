---
title: Query Language (DSL)
description: Complete reference for trawl's pipeline query language.
---

trawl uses a pipeline-oriented query language inspired by Splunk's SPL. Queries flow through stages separated by `|`, with each stage transforming the data for the next.

## Query structure

```
[search stage] | [pipe stage] | [pipe stage] ...
```

The search stage is optional. Pipelines can start with `|` for raw log access.

## Search stage

The search stage filters events before they enter the pipeline. Multiple conditions within a group are AND-joined.

### Field filters

```
service=nginx                    # exact match
status=200,301,404              # IN list (comma-separated)
status>=400                     # comparison
path=/api/*                     # glob pattern
message=/error.*/               # regex pattern (slash-delimited)
service="Activity Monitor"      # quoted values for spaces/special chars
```

**Operators:** `=`, `!=`, `>`, `>=`, `<`, `<=`

### Text search

```
error                           # bare word — substring match
-debug                          # negated — exclude matches
"connection refused"            # exact phrase
```

### Time filters

```
last=2h                         # units: s, m, h, d, w
last=7d
last=30m
```

### OR grouping

```
service=nginx OR service=apache
a b OR c d                      # implicit AND within groups: (a AND b) OR (c AND d)
```

## Pipe stages

### stats

Aggregate with optional grouping.

```
stats count()
stats count() by host
stats avg(duration) by status
stats count(), avg(duration) by service, host
stats avg(duration) as avg_duration
```

### where

Filter on computed values.

```
where count > 10
where avg_duration > 100
where status == 200 and count > 5
where host matches /prod-.*/
where x in (1, 2, 3)
```

### sort

Order results. Prefix with `-` for descending.

```
sort count                      # ascending
sort -count                     # descending
sort status, -count             # multi-field
```

### limit / head

Cap the number of results.

```
limit 20
head 20                         # alias for limit
```

### tail

Last N rows. Defaults to timestamp descending if no prior sort.

```
tail 5
```

### table / fields

Select output columns.

```
table host, status
fields host, status             # alias for table
```

### top

Most frequent values.

```
top 10 host                     # top 10 hosts by frequency
top 5 status by service         # top 5 statuses per service
```

### rare

Least frequent values.

```
rare 5 status                   # 5 rarest status codes
rare 3 host by service          # 3 rarest hosts per service
```

### drop

Exclude columns from output.

```
drop message
drop host, raw
```

### let / eval

Create computed fields.

```
let duration_ms = duration * 1000
eval status_class = status / 100    # eval is an alias for let
let is_error = status >= 400
```

### extract / rex

Extract fields from text using regex or key-value parsing.

**Named capture groups:**

```
extract "(?P<ip>\d+\.\d+\.\d+\.\d+)" from message
rex "(?P<code>[A-Z]+)" from raw    # rex is an alias for extract
```

**Key-value extraction:**

```
extract kv                      # from 'message' field
extract kv from raw             # from a specific field
```

### rename

Rename output columns.

```
rename service as svc
rename service as svc, host as hostname
```

### dedup

Remove duplicate rows. Keeps the most recent.

```
dedup                           # entire row
dedup host                      # by single field
dedup host, service             # by multiple fields
```

### timechart

Time-bucketed aggregation.

```
timechart span=5m count()
timechart span=1h count() by service
timechart span=30s count(), avg(duration)
```

### pivot

Pivot table transformation.

```
pivot count() on status
pivot avg(duration) on service by host
```

## Expressions

Used in `where`, `let`, and aggregation arguments.

| Type | Examples |
|------|----------|
| Literals | `42`, `1.5`, `"string"`, `true`, `false`, `null` |
| Field refs | `host`, `host.name`, `@timestamp` |
| Arithmetic | `+`, `-`, `*`, `/`, `%` |
| Comparison | `==`, `!=`, `>`, `>=`, `<`, `<=` |
| Logical | `and`, `or`, `not` |
| Pattern | `matches` (regex) |
| Membership | `x in (1, 2, 3)` |
| Grouping | `(count + 1) * 2` |

## Aggregation functions

### Basic stats

| Function | Description |
|----------|-------------|
| `count()` | Row count |
| `count(field)` | Non-null count |
| `avg(field)` | Mean |
| `sum(field)` | Total |
| `min(field)` | Minimum |
| `max(field)` | Maximum |

### Cardinality

| Function | Description |
|----------|-------------|
| `dc(field)` | Distinct count |
| `distinct_count(field)` | Alias for `dc` |

### Percentiles

| Function | Description |
|----------|-------------|
| `p50(field)` | Median (50th percentile) |
| `p90(field)` | 90th percentile |
| `p95(field)` | 95th percentile |
| `p99(field)` | 99th percentile |

### Positional and collection

| Function | Description |
|----------|-------------|
| `first(field)` | First value |
| `last(field)` | Last value |
| `values(field)` | List of distinct values |
| `list(field)` | Alias for `values` |
| `median(field)` | Median value |
| `stddev(field)` | Standard deviation |

## Scalar functions

Available in `let`/`eval` and `where` expressions.

### String functions

| Function | Description |
|----------|-------------|
| `lower(field)` | Lowercase |
| `upper(field)` | Uppercase |
| `length(field)` / `len(field)` | String length |
| `trim(field)` / `ltrim(field)` / `rtrim(field)` | Whitespace trimming |
| `replace(field, old, new)` | String replacement |
| `substr(field, start[, len])` | Substring extraction |

### Numeric functions

| Function | Description |
|----------|-------------|
| `abs(x)` | Absolute value |
| `ceil(x)` / `ceiling(x)` | Round up |
| `floor(x)` | Round down |
| `round(x[, n])` | Round to n decimal places |

### Conditional and type functions

| Function | Description |
|----------|-------------|
| `if(cond, then, else)` | Ternary conditional |
| `isnull(x)` / `isnotnull(x)` | Null checks |
| `coalesce(a, b, ...)` | First non-null value |
| `typeof(x)` | Value type name (returns `"VARCHAR"`, `"DOUBLE"`, `"TIMESTAMP"`, …) |
| `now()` | Current timestamp (timezone-naive, wall-clock UTC) |
| `tonumber(x)` | Cast to float (`null` on parse failure — mirrors `TRY_CAST AS DOUBLE`) |
| `tostring(x)` | Cast to string (`null` for null/array input) |

### Date and time functions

Date/time functions operate on **timestamps** — the `timestamp` field is stored as a timezone-naive `TIMESTAMP` in both the batch and streaming paths (any timezone offset is discarded at ingest, keeping wall-clock components).

| Function | Description |
|----------|-------------|
| `date_part(unit, ts)` | Extract a calendar component (returns integer or float for `epoch`) |
| `date_trunc(unit, ts)` | Truncate to start of period (returns timestamp) |
| `date_diff(unit, start, end)` | Count calendar-unit boundaries crossed (`end - start`); `week` is the exception (see note below) |
| `strftime(ts, fmt)` | Format timestamp as string (chrono `%`-codes) |
| `strptime(str, fmt)` | Parse string to timestamp (returns `null` on failure) |

**Note:** the argument order for `strftime` is `(timestamp, format)` — the opposite of C `strftime`. DuckDB's `STRFTIME` is overloaded and accepts this order directly, so it is emitted unchanged.

**Note:** `strptime` returns `null` when a value cannot be parsed against the format — a single unparseable value yields `null`, not a query error. This holds in both the batch path (emitted as DuckDB `TRY_STRPTIME`) and the streaming path.

#### Date/time unit allowlist

The `unit` argument to `date_part`, `date_trunc`, and `date_diff` must be a **string literal** from the allowed set. Non-literal expressions (field refs, computed values) and unlisted units are rejected before execution in both batch and streaming modes (batch validates at SQL-emit time, streaming at stream-plan compile time).

| Function | Allowed units |
|----------|--------------|
| `date_part` | `year`, `quarter`, `month`, `week`, `day`, `hour`, `minute`, `second`, `dow`, `doy`, `epoch` |
| `date_trunc` | `year`, `quarter`, `month`, `week`, `day`, `hour`, `minute`, `second` |
| `date_diff` | `year`, `quarter`, `month`, `week`, `day`, `hour`, `minute`, `second` |

`dow` = day of week (Sunday = 0 … Saturday = 6). `doy` = day of year (1–366). `epoch` = seconds since Unix epoch (float).

`date_trunc("week", ts)` truncates to **Monday midnight** (ISO 8601 week start).

`date_diff` counts boundary crossings between the two timestamps for `year`, `quarter`, `month`, `day`, `hour`, `minute`, and `second`. The `week` unit is the exception: DuckDB computes it as the whole number of days between the dates divided by 7 (integer division toward zero), **not** week-boundary crossings.

#### strftime/strptime format codes

Standard C `strftime` codes (`%Y`, `%m`, `%d`, `%H`, `%M`, `%S`, etc.) produce identical output in both batch (DuckDB) and streaming (chrono) paths. chrono operates on a timezone-naive timestamp, so it is **not** locale-dependent — but codes that depend on timezone or locale (`%Z`, `%z`, `%c`, `%x`, `%X`) render empty or fixed under chrono's naive semantics and can differ from DuckDB. Stick to explicit numeric codes for portable output.

Invalid format codes (e.g. `%Q`, or a trailing `%`) are rejected before execution in both paths when the format is a string literal — they no longer error in batch while silently nulling in streaming.

## Examples

```
# Errors in the last hour by service
level=error last=1h | stats count() by service | sort -count

# Slow requests by endpoint
status=200 last=24h | where duration > 1000 | stats avg(duration) by uri | sort -avg_duration | head 10

# 4xx/5xx rate by host
status>=400 last=2h | stats count() by host, status | where count > 10

# Extract IPs and count
"connection from" | rex "(?P<ip>\d+\.\d+\.\d+\.\d+)" from message | stats count() by ip | sort -count

# Time series of error rate
level=error OR level=fatal | timechart span=5m count() by service

# Dedup flapping alerts
service=monitoring | dedup host, alert_name

# Pivot status codes by host
last=1h | pivot count() on status by host

# Conditional field with null handling
* | eval msg_len = if(isnotnull(message), length(message), 0) | fields host, msg_len | head 10

# Distinct values per group
* | stats values(level), first(message) by service | head 10
```
