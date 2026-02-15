# fleet

self-hosted log collection, storage, and search platform for homelabs and small-to-medium infra. splunk-like DSL, zero licensing cost, single-node by design.

## stack

- **core**: rust workspace — parser, SQL emitter, DuckDB executor, daemon, CLI, TUI
- **ingestion**: vector → parquet (columnar, compressed, partitioned by hour)
- **query engine**: custom DSL → AST → DuckDB SQL (parameterized)
- **web ui**: rails 8 (thin proxy over fleetd HTTP API)
- **agent** (v2 scope): signed-template execution on managed endpoints, mTLS, ed25519 signing

## workspace layout

```
crates/
  fleet-core/     # DSL parser, AST, SQL emitter (pure, no I/O)
  fleet-engine/   # DuckDB integration, query execution
  fleet-auth/     # API keys, roles, SQLite-backed
  fleet-server/   # daemon (axum, unix socket, TCP+TLS)
  fleet-client/   # shared client library
  fleet-cli/      # unified CLI + TUI binary
  fleet-admin/    # admin CLI (key mgmt, templates, enrollment)
```

## key design decisions

- single-node only. no clustering, sharding, or multi-tenancy.
- pipeline-oriented DSL: `service:nginx level:error last:2h | stats count() by host | where count > 10`
- SQL injection prevention via parameterized queries + field allowlists
- agent tasks are cryptographically signed offline — compromised server can't create novel execution authority

## tooling

- **edition 2024**, resolver 3, rust-version 1.85 MSRV
- **clippy pedantic** + `unsafe_code = "forbid"` at workspace level
- **lefthook** pre-commit: `cargo fmt --check` + `cargo clippy -- -D warnings`
- **bacon** for continuous clippy-on-save (`bacon` or `bacon test`)
- **cargo-nextest** for testing, **cargo-insta** for snapshot tests
- **cargo-deny** for license/vulnerability auditing

## using fleet

- `fleet` (no subcommand) launches the interactive TUI
- `fleet query "dsl..."` executes a query and prints results (table for TTY, JSON for pipes)
- `fleet query --data '/path/*.parquet' "dsl..."` queries local parquet files (embedded mode)
- `fleet validate "dsl..."` checks DSL syntax without executing
- global flags: `--url`, `--token`, `--insecure`, `--token-file`, `--config`
- config file: `~/.config/fleet/config.toml` (shared by CLI and TUI modes)
- We are running a development server at https://localhost:5514 with a self-signed cert. You need to use the --insecure flag.
- Environment variables FLEET_URL (https://localhost:5514) AND FLEET_TOKEN (with an admin-scoped token) should be already set. Environment variables override the config files in ~/.config/fleet/

## docs

- `docs/overview.md` — project overview and architecture thesis
- `docs/initial_plan.md` — 15-phase implementation plan with detailed per-phase breakdowns

## DSL quick reference

### query structure

```
[search stage] | [pipe stage] | [pipe stage] ...
```

search stage is optional. pipelines can start with `|` for raw log access.

### search stage (pre-pipeline filtering)

**field filters**
```
service:nginx                    # exact match
status:200,301,404              # IN list (comma-separated)
status:>=400                    # comparison (>, >=, <, <=, !=)
path:/api/*                     # glob pattern
message:/error.*/               # regex pattern (slashes required)
service:"Activity Monitor"      # quoted values (for spaces/special chars)
```

**operators**: `:` (=), `:>`, `:>=`, `:<`, `:<=`, `:!=`

**text search**
```
error                           # bare word (substring match)
-debug                          # negated (exclude)
"connection refused"            # exact phrase
```

**time filters**
```
last:2h                         # units: s, m, h, d, w
last:7d
last:30m
```

**OR grouping**
```
service:nginx OR service:apache # OR-separated groups
a b OR c d                      # implicit AND within groups: (a AND b) OR (c AND d)
```

### pipe stages

**stats** — aggregation with optional grouping
```
stats count()
stats count() by host
stats avg(duration) by status
stats count(), avg(duration) by service, host
stats avg(duration) as avg_duration
```

**where** — filter on computed values
```
where count > 10
where avg_duration > 100
where status == 200 and count > 5
where host matches /prod-.*/
where x in (1, 2, 3)
```

**sort** — order results (`-` prefix for descending)
```
sort count                      # ascending
sort -count                     # descending
sort status, -count             # multi-field
```

**limit** / **head** — cap result count
```
limit 20
head 20                          # SPL alias for limit
```

**tail** — last N rows (defaults to timestamp DESC if no prior sort)
```
tail 5
```

**table** / **fields** — select output columns
```
table host, status
fields host, status              # SPL alias for table
```

**top** — most frequent values
```
top 10 host                     # top 10 hosts by frequency
top 5 status by service         # top 5 statuses per service
```

**rare** — least frequent values
```
rare 5 status                   # 5 rarest status codes
rare 3 host by service          # 3 rarest hosts per service
```

**drop** — exclude columns
```
drop message
drop host, raw
```

**let** / **eval** — computed/derived fields
```
let duration_ms = duration * 1000
eval status_class = status / 100 # SPL alias for let
let is_error = status >= 400
```

**extract** / **rex** — field extraction

regex (named groups):
```
extract "(?P<ip>\d+\.\d+\.\d+\.\d+)" from message
rex "(?P<code>[A-Z]+)" from raw  # SPL alias for extract
```

key-value pairs:
```
extract kv                      # from 'message' field
extract kv from raw             # from specific field
```

**rename** — rename columns
```
rename service as svc
rename service as svc, host as hostname
```

**dedup** — remove duplicates (keeps most recent)
```
dedup                           # entire row
dedup host                      # by single field
dedup host, service             # by multiple fields
```

**timechart** — time-bucketed aggregation
```
timechart span=5m count()
timechart span=1h count() by service
timechart span=30s count(), avg(duration)
```

**pivot** — pivot table transformation
```
pivot count() on status
pivot avg(duration) on service by host
```

### expressions (in `where`, `let`, aggregations)

**literals**: `42`, `1.5`, `"string"`, `true`, `false`, `null`

**field refs**: `host`, `host.name`, `@timestamp`

**arithmetic**: `+`, `-`, `*`, `/`, `%`

**comparison**: `==`, `!=`, `>`, `>=`, `<`, `<=`

**logical**: `and`, `or`, `not`

**pattern**: `matches` (regex)

**lists**: `x in (1, 2, 3)`

**precedence**: parens supported `(count + 1) * 2`

### aggregation functions

**basic stats**
- `count()` — row count
- `count(field)` — non-null count
- `avg(field)` — mean
- `sum(field)` — total
- `min(field)` — minimum
- `max(field)` — maximum

**cardinality**
- `dc(field)` or `distinct_count(field)` — unique value count

**percentiles**
- `p50(field)` — median
- `p90(field)` — 90th percentile
- `p95(field)` — 95th percentile
- `p99(field)` — 99th percentile

**positional / collection**
- `first(field)` — first value
- `last(field)` — last value
- `values(field)` / `list(field)` — list of distinct values
- `median(field)` — median value
- `stddev(field)` — standard deviation

### scalar functions (in `let`/`eval`, `where`, expressions)

**string**
- `lower(field)` — lowercase
- `upper(field)` — uppercase
- `length(field)` / `len(field)` — string length
- `trim(field)` / `ltrim(field)` / `rtrim(field)` — whitespace trimming
- `replace(field, old, new)` — string replacement
- `substr(field, start[, len])` — substring extraction

**numeric**
- `abs(x)` — absolute value
- `ceil(x)` / `ceiling(x)` — round up
- `floor(x)` — round down
- `round(x[, n])` — round to n decimal places

**conditional / type**
- `if(cond, then, else)` — ternary conditional
- `isnull(x)` / `isnotnull(x)` — null checks
- `coalesce(a, b, ...)` — first non-null value
- `typeof(x)` — value type name
- `now()` — current timestamp

### example queries

```
# errors in the last hour by service
level:error last:1h | stats count() by service | sort -count

# slow requests by endpoint
status:200 last:24h | where duration > 1000 | stats avg(duration) by uri | sort -avg_duration | head 10

# 4xx/5xx rate by host
status:>=400 last:2h | stats count() by host, status | where count > 10

# extract IPs and count
"connection from" | rex "(?P<ip>\d+\.\d+\.\d+\.\d+)" from message | stats count() by ip | sort -count

# time series of error rate
level:error OR level:fatal | timechart span=5m count() by service

# dedup flapping alerts
service:monitoring | dedup host, alert_name

# pivot status codes by host
last:1h | pivot count() on status by host

# last 5 events with renamed columns
* | rename service as svc, host as hostname | tail 5

# conditional field + null handling
* | eval msg_len = if(isnotnull(message), length(message), 0) | fields host, msg_len | head 10

# distinct values per group
* | stats values(level), first(message) by service | head 10
```
