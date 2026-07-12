# trawl

self-hosted log collection, storage, and search platform for homelabs and small-to-medium infra. splunk-like DSL, zero licensing cost, single-node by design.

## stack

- **core**: rust workspace — parser, SQL emitter, DuckDB executor, daemon, CLI, TUI
- **ingestion**: vector → parquet (columnar, compressed, partitioned by hour)
- **query engine**: custom DSL → AST → DuckDB SQL (parameterized)
- **web ui**: leptos 0.8 CSR SPA (`trawl-web-ui`) served by the `trawl-web` session proxy (cookie sessions → bearer tokens)
- **shared fleet substrate** (ADR-0030, consumed by coastwatch via sibling path deps): `fleet-auth` (postgres keystore + session AEAD), `fleet-ui` (leptos design system), `fleet-admin` (ops CLI)
- **agent** (v2 scope): signed-template execution on managed endpoints, mTLS, ed25519 signing — not yet started

## workspace layout

```
crates/
  trawl-core/            # DSL parser, AST, SQL emitter (pure, no I/O)
  trawl-engine/          # DuckDB integration, query execution
  trawl-auth/            # history/saved/schedule stores, SQLite-backed (keystore half retired — trawld verifies keys against fleet-auth postgres since ADR-0004 slice 1; crate dies in slice 3)
  trawl-api/             # shared wire types (request/response structs)
  trawl-config/          # shared config.toml types (no I/O)
  trawl-server/          # daemon (axum, HTTPS via tokio-rustls)
  trawl-client/          # typed async HTTP client library
  trawl-cli/             # unified CLI + TUI binary
  trawl-admin/           # admin CLI (TLS cert generation only — key mgmt lives in fleet-admin)
  trawl-web/             # browser-facing session proxy (serves SPA, cookie → bearer)
  trawl-web-ui/          # leptos 0.8 CSR SPA (wasm32)
  trawl-dashboard/       # shared ratatui dashboard rendering
  trawl-crashdump/       # minidump capture for trawld (linux fatal-signal handler)
  fleet-auth/            # postgres-backed keystore + session cookie AEAD + axum middleware (ADR-0030)
  fleet-ui/              # shared leptos design tokens + components for fleet apps (wasm32)
  fleet-admin/           # fleet keystore ops CLI (migrations, session keys, key lifecycle)
  coastwatch-api-types/  # vendored coastwatch API wire types
```

## key design decisions

- single-node only. no clustering, sharding, or multi-tenancy.
- pipeline-oriented DSL: `service=nginx level=error last=2h | stats count() by host | where count > 10`
- SQL injection prevention via parameterized queries + field allowlists
- agent tasks are cryptographically signed offline — compromised server can't create novel execution authority
- **real-time event bus**: ingested events are published to a `broadcast::channel`-backed bus and stored in a hot buffer, making them queryable within milliseconds of ingest (before WAL compaction to parquet)
- **hot buffer**: batch-keyed in-memory store that makes fresh events visible to ALL queries via `UNION ALL BY NAME` with the parquet source; drained automatically after compaction
- **live streaming**: SSE endpoint uses `CompiledFilter` (in-memory DSL matcher) against the event bus for real-time event delivery, with back-pressure notifications via `StreamEvent::Lagged`

## tooling

- **edition 2024**, resolver 3, rust-version 1.88 MSRV
- **clippy pedantic** + `unsafe_code = "forbid"` at workspace level
- **lefthook** pre-commit: `cargo fmt --check` + `cargo clippy -- -D warnings`
- **bacon** for continuous clippy-on-save (`bacon` or `bacon test`)
- **cargo-nextest** for testing, **cargo-insta** for snapshot tests
- **cargo-deny** for license/vulnerability auditing

## using trawl

### binaries

- **trawl CLI binary**: `target/debug/trawl` (after `cargo build -p trawl-cli`)
- the binary name is `trawl`, NOT `trawl-cli` — the crate is `trawl-cli` but the binary is `trawl`
- the server binary is `target/debug/trawld` (crate `trawl-server`)
- admin binary is `target/debug/trawl-admin` (crate `trawl-admin`)
- web proxy binary is `target/debug/trawl-web` (crate `trawl-web`) — serves the SPA + translates cookie sessions to bearer tokens
- web UI crate is `trawl-web-ui` — leptos 0.8 CSR SPA, built via `trunk build`

### web UI deploy

the web UI ships as a single binary: `trawl-web` with the SPA baked in
via `rust-embed`. the canonical build is:

```sh
cargo xtask build-web --release
# produces target/release/trawl-web with dist/ embedded
```

the xtask runs `trunk build --release` inside `crates/trawl-web-ui/`
then `cargo build --release -p trawl-web` so rust-embed picks up the
fresh SPA. `cargo xtask` is aliased in `.cargo/config.toml`.

for local iteration there are two faster flows:
- **trunk serve** (SPA hot reload): `cd crates/trawl-web-ui && trunk serve` — proxies `/api/*` to a separately-run `trawl-web` on :8090. full docs in `Trunk.toml`.
- **env override**: `TRAWL_WEB_SPA_DIR=$(pwd)/crates/trawl-web-ui/dist cargo run -p trawl-web` — `trawl-web` serves a pre-built `dist/` from disk instead of its embedded copy. lets you rebuild the SPA without recompiling the binary.

### packaging

`trawl-web` ships by default in both distribution channels:

- **debian**: bundled inside the `trawl-server` .deb alongside `trawld` + `trawl-admin`. postinst generates `/var/lib/trawl/web.cookie` (32 random bytes) and enables+starts `trawl-web.service` — the proxy listens on `127.0.0.1:8090` out of the box. the `[web]` block in `/etc/trawl/trawld.toml` is shared with trawld.
- **helm**: sidecar container in the same StatefulSet pod as trawld, guarded by `web.enabled` (default true). cookie key is stored in a chart-managed Secret and preserved across upgrades via helm's `lookup` function. ingress defaults to the web sidecar (`ingress.backend: web`) — switch to `ingress.backend: trawld` for bearer-token api clients.

both distributions pre-generate/preserve the cookie secret so sessions survive restart.

CLI clients (`trawl query`, `trawl-client`) and vector still hit **trawld directly** on port 5514 — the proxy only speaks cookies and hard-404s `/api/v1/ingest`.

### environments

config uses named profiles (`~/.config/trawl/config.toml`):
- **base** (no `--profile`): live homelab server at `trawl-01.lab.ktle.net:5514`
- **dev** (`--profile dev` or `TRAWL_PROFILE=dev`): localhost:5514 with self-signed cert

env vars and CLI flags override profile settings.

### CLI modes

**TUI** (interactive):
```
trawl                               # launches TUI against base (live) server
trawl -p dev                        # TUI against dev server
```

**query** (execute and print):
```
trawl query "dsl..."                             # auto-detect output: table for TTY, JSON for pipe
trawl query -p dev "dsl..."                      # query dev server
trawl query -f table "dsl..."                    # force table output
trawl query -f json "dsl..."                     # force JSON (one object per line, ndjson)
trawl query -f csv "dsl..."                      # CSV output (with formula injection protection)
```

**embedded mode** (no server, queries parquet files directly):
```
trawl query --data '/path/*.parquet' "dsl..."    # query local parquet files
trawl query --data 'data/**/*.parquet' "* | stats count() by service"
```

**validate** (syntax check, hits server for validation endpoint):
```
trawl validate "dsl..."                          # validates against base server
trawl validate -p dev "dsl..."                   # validates against dev server
```

### global flags

| flag | env var | description |
|------|---------|-------------|
| `-p, --profile <NAME>` | `TRAWL_PROFILE` | named profile from config (overrides `[server]`) |
| `--url <URL>` | `TRAWL_URL` | server URL (default: `https://localhost:5514`) |
| `--token <TOKEN>` | `TRAWL_TOKEN` | API token (direct value) |
| `--insecure` | `TRAWL_INSECURE` | accept self-signed TLS certificates |
| `-c, --config <PATH>` | — | config file path (default: `~/.config/trawl/config.toml`) |

### output formats

- **table**: pretty-printed box-drawing table with row count footer (default for TTY)
- **json**: one JSON object per row, ndjson-style (default for pipes)
- **csv**: RFC 4180 with formula injection protection (string values starting with `=`, `+`, `-`, `@`, `\t`, `|` are prefixed with `'`)
- **parquet**: DuckDB COPY TO with Snappy compression (file output only, requires `--output <path>`)

### common dev examples

```sh
# recent errors by service (live server, base profile)
trawl query "level=error last=1h | stats count() by service | sort -count | head 10"

# same query against dev server
trawl query -p dev "level=error last=1h | stats count() by service | sort -count | head 10"

# browse all data (table output)
trawl query -f table "* | head 5 | fields timestamp, host, service, level, message"

# pipe JSON to jq for ad-hoc processing
trawl query "last=1h | stats count() by service" | jq '.service'

# export to CSV file
trawl query -f csv "last=24h | stats count() by service, level" > report.csv

# validate a query without executing
trawl validate "level=error | stats count() by host"

# query local parquet files (no server needed)
trawl query --data 'data/**/*.parquet' "* | stats count() by service | sort -count"
```

### HTTP API

the trawl server exposes a REST API. all routes under `/api/v1` except `/health` and `/ingest` require bearer token auth:

| method | path | description |
|--------|------|-------------|
| `GET` | `/api/v1/health` | health check (unauthenticated) |
| `POST` | `/api/v1/query` | execute a DSL query |
| `POST` | `/api/v1/validate` | validate DSL syntax |
| `GET` | `/api/v1/schema` | get column names and types |
| `GET` | `/api/v1/schema/values/{field}` | get distinct values for a field |
| `GET` | `/api/v1/queries` | list running queries |
| `DELETE` | `/api/v1/queries/{id}` | cancel a running query |
| `GET` | `/api/v1/stats` | server statistics |
| `GET` | `/api/v1/dashboard` | full dashboard snapshot (admin only) |
| `GET` | `/api/v1/whoami` | token identity and permissions |
| `GET` | `/api/v1/history` | query execution history |
| `GET` | `/api/v1/saved` | list saved queries |
| `POST` | `/api/v1/saved` | create a saved query |
| `PUT` | `/api/v1/saved/{id}` | update a saved query |
| `DELETE` | `/api/v1/saved/{id}` | delete a saved query |
| `PUT` | `/api/v1/saved/{id}/schedule` | upsert schedule on saved query |
| `GET` | `/api/v1/saved/{id}/schedule` | get schedule |
| `DELETE` | `/api/v1/saved/{id}/schedule` | delete schedule |
| `GET` | `/api/v1/saved/{id}/runs` | list report runs (paginated) |
| `GET` | `/api/v1/saved/{id}/runs/{run_id}` | get report run with result data |
| `POST` | `/api/v1/export` | export query results (csv/json/parquet) |
| `GET` | `/api/v1/stream` | SSE stream of query results |
| `POST` | `/api/v1/ingest` | ingest log events (JSON array/ndjson, optional gzip) |
| `GET` | `/metrics` | prometheus metrics (outside /api/v1, unauthenticated) |

## docs

the docs site lives under `docs/` (astro starlight). key pages:

- `docs/src/content/docs/architecture/overview.md` — design principles, components, comparison to splunk/datadog
- `docs/src/content/docs/architecture/data-flow.md` — ingestion pipeline, compaction, storage layout, query execution
- `docs/src/content/docs/about/roadmap.md` — what's shipped, what's next, what's out of scope
- `docs/src/content/docs/reference/{cli,dsl,api,configuration}.md` — user-facing reference
- `docs/src/content/docs/getting-started/{index,first-query,vector-integration}.md` — onboarding

repo-root docs:

- `README.md` — project overview
- `CHANGELOG.md` — release history
- `TUI_ROADMAP.md` — opinionated UX review for the TUI

## DSL quick reference

### query structure

```
[search stage] | [pipe stage] | [pipe stage] ...
```

search stage is optional. pipelines can start with `|` for raw log access.

### search stage (pre-pipeline filtering)

**field filters**
```
service=nginx                    # exact match
status=200,301,404              # IN list (comma-separated)
status>=400                     # comparison (>, >=, <, <=, !=)
path=/api/*                     # glob pattern
message=/error.*/               # regex pattern (slashes required)
service="Activity Monitor"      # quoted values (for spaces/special chars)
```

**operators**: `=`, `!=`, `>`, `>=`, `<`, `<=`

**text search**
```
error                           # bare word (substring match)
-debug                          # negated (exclude)
"connection refused"            # exact phrase
```

**time filters**
```
last=2h                         # units: s, m, h, d, w
last=7d
last=30m
```

**OR grouping**
```
service=nginx OR service=apache # OR-separated groups
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
level=error last=1h | stats count() by service | sort -count

# slow requests by endpoint
status=200 last=24h | where duration > 1000 | stats avg(duration) by uri | sort -avg_duration | head 10

# 4xx/5xx rate by host
status>=400 last=2h | stats count() by host, status | where count > 10

# extract IPs and count
"connection from" | rex "(?P<ip>\d+\.\d+\.\d+\.\d+)" from message | stats count() by ip | sort -count

# time series of error rate
level=error OR level=fatal | timechart span=5m count() by service

# dedup flapping alerts
service=monitoring | dedup host, alert_name

# pivot status codes by host
last=1h | pivot count() on status by host

# last 5 events with renamed columns
* | rename service as svc, host as hostname | tail 5

# conditional field + null handling
* | eval msg_len = if(isnotnull(message), length(message), 0) | fields host, msg_len | head 10

# distinct values per group
* | stats values(level), first(message) by service | head 10
```
