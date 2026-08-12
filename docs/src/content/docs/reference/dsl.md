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
host="db host"                  # quoted values for spaces/special chars
env=prod                        # environment (path-pruned)
```

**Operators:** `=`, `!=`, `>`, `>=`, `<`, `<=`

#### Pinned comparison semantics

On a server, every stored field carries a type pin in the field catalog
(the envelope columns are pinned on install; custom fields pin at first
typed sight). Two rules follow, and they are one rule seen from either
end:

- **What is stored** is the value's reading under the pin — but only when
  that reading is *value-preserving*. A cast that would silently alter the
  value is refused: the column holds NULL, the disagreement is recorded as
  a conflict (`trawl schema conflicts`), and the original text stays
  findable in `_raw`. Spelling drift is not alteration, so `"0404"` under
  a BIGINT pin stores as `404`, but `"1.5"` stores as NULL rather than
  rounding to `2`.
- **What matches** is decided by the pin, not by the shape of the query
  literal, so a comparison means the same thing however you spell it.

An event answers the same way the moment it lands as it will hours later:
the hot buffer conforms through the identical expression compaction writes
with. Live tail (SSE) applies the same rules again, so a streamed query,
its batch form, and the same query after the compactor runs all agree
event for event.

##### The numeric reading

Both VARCHAR-pinned numeric rungs below read the column *and* the literal
through the same `DECIMAL(38,6)` cast, so the two sides can never disagree
about what a string means. That space is **exact for every 64-bit
integer** and out to 10^32 — `id=1737000000123456789` matches that id and
no neighbour.

What has a reading: surrounding whitespace is ignored (`" 200"` is 200),
`_` between digits is a separator (`"200_000"` is 200000), leading zeros
are fine (`"0404"` is 404), and `"+5"`, `"1."`, `".5"`, `"1e3"` and
`"1E-3"` all read.

What has **no** reading — and no reading means *unknown*, never a false
match, and `NOT` cannot invert it into one: `"nan"`, `"inf"`,
`"infinity"`, radix prefixes like `"0x10"`, grouped digits like
`"1,000"`, empty or blank text, and any magnitude at or above 10^32.
Fractions quantize at 10^-6 (rounded half away from zero), so two values a
nanosecond apart compare equal — sub-microsecond ordering is not something
a VARCHAR-pinned field can express.

##### The rules, per pin

- **VARCHAR-pinned field, `=` / `!=` / IN list** — compares **as text**:
  `status=accepted` matches the stored string `"accepted"`, exactly. A
  *numeric* literal matches the exact text **or** any spelling of the
  same number: `status=200` finds `"200"`, `"0200"` and `"200.0"`, but
  not `"accepted"` or `"404"`. The numeric half is not optional — the
  text a number is stored under depends on the batch it arrived in (one
  fractional value anywhere in the batch stores `200` as `"200.0"`), and
  live tail must answer the same as a batch query. `!=` is the exact
  complement: `status!=200` returns `"accepted"` and every other
  non-200 value. A literal the numeric space can't read (`status=nan`,
  `id=1e40`) falls back to the text comparison alone.
- **VARCHAR-pinned field, ordered comparison with a numeric literal** —
  compares **numerically** in that same space: `status>=400` matches
  `"404"`/`"500"`, and values with no numeric reading like `"accepted"`
  simply don't match (they never error the query). Such a value is
  *unknown*, not *false*, exactly as in SQL — so `NOT status>=400`
  doesn't match those rows either. A *literal* with no reading
  (`dur>1e40`) matches nothing at all, on either side.
- **VARCHAR-pinned field, ordered comparison with a non-numeric
  literal** — lexical string comparison, unchanged.
- **Integer-pinned field, glob or regex** — matches the **stored
  integer's** text form, which is not always how the event spelled it:
  `status=4*` finds 404 in a BIGINT column, and a wire `"0404"` is stored
  as 404, so it matches `status=4*` and *not* `status=0*`. The same holds
  for `"4.0"`, `" 200"`, `"200_000"` and `"1e3"` — value-preserving
  spellings, stored as the integer. A value the cast would *rewrite* —
  `"1.5"` (rounded to 2), or `"0x10"` (read as 16, but a spelling DuckDB
  would never write back) — and one it can't read at all (`"accepted"`)
  are stored as NULL and are *unknown*, not false.
- **Boolean-pinned field, glob or regex** — matches `true` or `false`,
  lowercase, and only genuine ones. DuckDB's boolean vocabulary is wider
  than what it writes back (`"TRUE"`, `"t"`, `"yes"`, `"1"` are all inside
  the cast), but none of those survive the round trip, so they are stored
  as NULL and counted as conflicts — the stored column holds exactly the
  values the wire spelled `true` or `false`. `flag=/^true$/` matches
  those; `flag=TRUE*` matches nothing. A NULL is *unknown*, not false.
- **Double-pinned field, glob or regex** — matches the value's text form
  too, but a double's text form is not what the event's JSON looked like:
  it always carries a fraction, and switches to a signed, two-digit
  exponent outside `1e-4 … 1e16` (`200.0`, `0.0`, `-3.0`, `1e-07`,
  `1.2345678901234568e+17`). So `dur=/^200$/` matches nothing while
  `dur=/^200\.0$/` matches — the same on both sides, batch and live.
  Pinning DOUBLE *means* accepting DOUBLE's precision, so this is the one
  rung with no round-trip guard: anything the cast reads is stored. A
  value with no numeric reading is stored as NULL and is *unknown*, not
  false.
- **Timestamp-pinned field, glob or regex** — matches the **RFC 3339
  UTC-microsecond form** of the instant, always with a `T` separator, six
  fractional digits and a trailing `Z`
  (`2026-01-15T09:00:00.000000Z`). The parse is **zone-aware**: an offset
  in the wire text is *applied*, so `2026-01-15T09:00:00+05:30` is stored
  and matched as `2026-01-15T03:30:00.000000Z` — `_time=/T03:30/`, not
  `/T09:00/`. Text without an offset is read as UTC, a bare date is
  midnight, and fractions truncate at six digits. So `_time=2026-01-15*`,
  `_time=/T09:/` and `_time=/\.123456Z$/` all mean the same thing in a
  batch query and in live tail. A value with no timestamp reading is
  stored as NULL and is *unknown*, not false — `NOT _time=/T09:/` doesn't
  match it either. (The envelope's own `_time`/`_ingested` are already
  canonicalized to UTC at ingest, so this only changes how a *custom*
  timestamp-pinned field reads.)
- **Typed-pinned field (BIGINT / DOUBLE / BOOLEAN / TIMESTAMP),
  comparison** — compares what the column **stores**, which is the
  conformed value: a wire `1.5` under a BIGINT pin is NULL there, so
  `duration>1` does not match it, `NOT duration>1` does not either
  (*unknown*, not false), and `duration!=2` does — a NULL column matches
  `!=` by design. The literal binds exactly as on an unpinned field
  (the column already has the pinned type) and DuckDB reads it against
  that type, so `flag=TRUE` and `flag=yes` both match a stored `true`
  even though those same texts *stored* conform to NULL, and an offset
  spelled in a timestamp literal is ignored where the same offset in a
  stored value is applied. Live tail answers the same way, event for
  event.
- Every comparison on an **unpinned** field keeps plain literal-driven
  behavior.

:::caution[Changed in the ADR-0011 release]
The zone-aware timestamp parse applies to values conformed **from this
release onward**. A custom timestamp field that received offset-bearing
values before it may hold wall-clock instants in already-compacted
partitions; those are not rewritten, so a glob over such a field can span
both readings until the old partitions age out.
:::

#### Missing fields and nulls

A field an event doesn't carry is a NULL column, and a comparison against
NULL is **unknown** — neither true nor false. This is SQL's rule and it
holds everywhere: batch queries, exports, and live tail, on pinned and
unpinned fields alike. Only a *true* row is returned, so an unknown one is
filtered out. Two consequences are worth knowing before you write an alert:

- `f!=x` **matches events that carry no `f`** (and events whose `f` is
  null). Its emitted form is `("f" != ? OR "f" IS NULL)` — the one total
  comparison. On live tail that is a wide net: bus events carry the
  envelope plus whatever their sender sent, so `f!=x` over a sparse custom
  field streams nearly everything. Pair it with `f=*` to require the field.
- `NOT f=x` **does not match events that carry no `f`** — `NOT (NULL)` is
  NULL, which is unknown, which is filtered out. If you want "events
  missing `f`, plus events where it isn't `x`", write `f!=x`, not
  `NOT f=x`. The same holds for `NOT level=...` when an event has no
  `severity`, and for `NOT <bare term>` when it has no `message`/`_raw`.

:::caution[Changed in the ADR-0011 release]
Live tail previously treated a missing field as *false* rather than
unknown, so `f!=x` matched nothing on such events and `NOT f=x` matched
all of them — the opposite of what the same query returned from
`/api/v1/query`. Live tail now agrees with the batch answer. Alerts built
on `NOT f=x` to catch events missing a field need rewriting as `f!=x`.
:::

Two deliberate boundaries:

- **Numeric-literal detection is by content, not quoting**: the parser
  discards quote provenance, so `status>"400"` and `status>400` are the
  same query.
- **Embedded mode (`--data`) and the pipeline `| where` stage stay
  literal-driven** — there is no catalog behind `--data`, and `| where`
  is a typed expression evaluated after the search stage. `| where
  status > 400` over a VARCHAR-pinned column can therefore still error
  where the search-stage `status>400` filters cleanly.

### Severity: the `level` alias

`level` is a **query alias for the numeric `severity` column** (OTel
SeverityNumber 1-24). Name tokens are matched case-insensitively and
compile to band predicates:

```
level=error                     # severity BETWEEN 17 AND 20 (the ERROR band)
level!=info                     # NOT BETWEEN 9 AND 12, or severity IS NULL
level=warn,error                # either band
level>=warn                     # severity >= 13 (the token's exact number)
| where level == "error"        # same band predicate, in a pipe stage
```

- Equality/IN match the whole band containing the token (`notice` falls
  inside the INFO band).
- Ordered comparisons use the token's exact number (`warn` = 13,
  `error` = 17, ...).
- Valid tokens: `trace`/`t`, `debug`/`d`, `info`/`i`, `notice`,
  `warn`/`warning`/`w`, `error`/`err`/`e`, `fatal`/`critical`/`crit`/`f`,
  `alert`, `emerg`/`panic`. Anything else (or a glob/regex on `level`) is
  a query error — match the original spelling with
  `severity_text="..."` instead.
- `level` works in search-stage filters and in `where` comparisons
  against a token literal — both compile to the same band predicate, and
  live tail (SSE) evaluates them identically to a batch query.
- Everywhere else — projections, `stats by`, `sort`, `dedup`, `rename`,
  `let` arithmetic — naming `level` is a query error, because there is no
  stored `level` column to read or write. Use `severity` (the number) or
  `severity_text` (the original text) instead. The error is deliberate:
  emitting `level` verbatim would ask the database for a column that does
  not exist, and a pre-cutover saved query would come back empty rather
  than say so.

### Time aliases

`timestamp` and `@timestamp` are query aliases for the physical `_time`
column — all three resolve identically.

### Text search

```
error                           # bare word — substring match
-debug                          # negated — exclude matches
"connection refused"            # exact phrase
```

Bare-word and phrase search match the `message` column **and** `_raw`,
so content that was parsed away is still findable. Negation excludes an
event when either column matches.

Because `_raw` holds the most original form of the event, searching it is
**whole-event search**, and what "whole event" means depends on who filled
it:

- a collector that sent its own string `_raw` — the pre-parse line, matched
  as text;
- everything else — the server's JSON serialization of the event as it
  arrived, so a term matches anywhere in that object: another field's
  **value** (`nginx` finds an event with `service=nginx`, even when
  `message` never says it) and a field **name** (`debug` finds an event
  carrying `debug_mode`, and `-debug` therefore excludes it).

That is the point of a bare word — find the event without knowing which
field holds the term. When you do know, filter the field and `_raw` is
never consulted:

```
message=/debug/                 # regex, message only
message=*debug*                 # glob, message only (case-sensitive)
service=nginx                   # exact field match
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

### Nested fields (JSON)

Nested objects and arrays are stringified at ingest: a field like `k8s: {"pod": "x", "ns": "default"}` is stored as its JSON text in a `VARCHAR` column (the field catalog pins it as such), never as a `STRUCT`. Reach into it with `json_extract_string`:

```
service=kubelet | eval pod = json_extract_string(k8s, "$.pod") | where isnotnull(pod)
```

Bare text search also matches inside the stringified value (it is part of `_raw` and of the column's text).

### Date and time functions

Date/time functions operate on **timestamps** — the `timestamp` field is stored as a timezone-naive `TIMESTAMP` in both the batch and streaming paths. Ingest first canonicalizes the value to UTC, so an incoming offset is *applied* (`12:00:00+05:30` becomes `06:30:00Z`) rather than dropped in favour of its wall-clock components; the naive timestamp everything downstream sees is therefore UTC. See [Data flow](/architecture/data-flow/#timestamp-canonicalization) for the accepted input grammar.

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

**Partial formats:** `strptime` fills the components a format omits from a `1900-01-01 00:00:00` base, identically in the batch (DuckDB) and streaming paths. A **date-only** format (e.g. `%Y-%m-%d`) yields midnight (`00:00:00`); a **time-only** format (e.g. `%H:%M:%S`) yields the `1900-01-01` base date; **year-only** (`%Y` → `2023-01-01 00:00:00`), **year-month** (`%Y-%m` → `2023-11-01 00:00:00`), a bare **month-day** (`%m-%d` → `1900-11-07 00:00:00`), and a date with an *incomplete* time (`%Y-%m-%d %H` → `…14:00:00`) all fill the same way. Exotic or locale-dependent codes follow chrono in the streaming path and may differ from DuckDB: a bare two-digit year (`%y` alone) yields `null` where DuckDB fills the base year, and timezone-offset codes (`%Z`/`%z`) keep chrono's wall-clock time rather than normalizing to UTC. (A two-digit year *with* a month/day, like `%y-%m-%d`, resolves via chrono's pivot and matches DuckDB.)

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
* | stats values(severity_text), first(message) by service | head 10
```
