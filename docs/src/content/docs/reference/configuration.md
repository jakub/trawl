---
title: Configuration
description: Complete reference for trawld server and trawl client configuration.
---

## Server configuration

The trawl server is configured via a TOML file, typically at `~/.trawl/trawld.toml` (local) or `/etc/trawl/trawld.toml` (system package).

```bash
trawld --config /path/to/trawld.toml
# or
TRAWL_CONFIG=/path/to/trawld.toml trawld
```

### Server environment variables

| Variable | Description |
|----------|-------------|
| `TRAWL_CONFIG` | Config file path |
| `TRAWL_QUERY_LOG` | ndjson query debug log path |
| `FLEET_DATABASE_URL` | Fleet keystore postgres URL (overrides `[auth] database_url`) |
| `TRAWL_DATABASE_URL` | Trawl app-state postgres URL (overrides `[storage] database_url`) |

`DATABASE_URL` is **not** read by trawld (it is fleet-admin's variable and the sqlx test harness's).

### Syntax notes

**Byte sizes** accept human-readable strings with binary (1024-based) multipliers:
```toml
max_body_bytes = "16M"      # 16 × 1024 × 1024 = 16,777,216 bytes
hot_buffer_max_bytes = "100M"
min_free_disk_bytes = "1G"
```

Raw integers are also accepted for backward compatibility.

**Paths** support tilde expansion: `"~/.trawl/data"` expands to `$HOME/.trawl/data` at startup.

### `[server]`

HTTPS listener, query limits, TLS, and rate limiting.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `http_addr` | string | `"127.0.0.1:8080"` | HTTPS listen address |
| `timeout_secs` | integer | `30` | Absolute query request deadline, including queue waits (seconds) |
| `max_concurrent_queries` | integer | *num CPUs* | DuckDB executor pool size |
| `max_result_rows` | integer | `100000` | Max rows before query is rejected |
| `max_export_rows` | integer | `1000000` | Max rows for export (bypasses `max_result_rows`) |
| `max_request_body_bytes` | byte size | `"128K"` | Request body limit (non-ingest routes) |
| `max_concurrent_requests` | integer | `256` | Total concurrent HTTP requests (503 when exceeded) |
| `shutdown_drain_secs` | integer | `30` | Graceful shutdown timeout for in-flight requests |
| `max_sse_connections` | integer | `32` | Concurrent SSE streams (429 when exceeded) |
| `schema_cache_ttl_secs` | integer | `60` | Cache duration for `GET /api/v1/schema` |
| `max_query_history` | integer | `1000` | Completed queries kept in memory (ring buffer) |
| `tls_cert_path` | path | *(auto-generated)* | PEM certificate path |
| `tls_key_path` | path | *(auto-generated)* | PEM private key path |
| `tls_reload_interval_secs` | integer | `300` | Poll cert/key files for changes; `0` disables |
| `cors_allowed_origins` | string array | `[]` | Allowed CORS origins; empty disables CORS |
| `query_log` | path | *(none)* | ndjson debug log (one entry per query execution) — see [the query debug log](#the-query-debug-log) |
| `query_log_max_bytes` | byte size | `"100M"` | Query debug log size cap with single-file rollover to `<path>.1`; `0` disables rollover |
| `log_file` | path | *(none)* | JSON log file; superseded by `internal_telemetry` |

For size, duration, and retention knobs, `0` disables the limit only where
the field says so. In particular, `server.query_log_max_bytes = 0` disables
query-log rollover. A global `retention.max_age_days = 0` keeps data forever
in every env that has no `[retention.env.<name>]` entry of its own; an env
with a finite entry still ages out under it. A per-env
`retention.env.<name>.max_age_days = 0` keeps that one env's data forever as
far as age goes. Either `0` leaves the `/api/v1/schema` listing with no
retention window at all.
`ingest.telemetry_buffer_max_bytes` is the deliberate exception: it must be a
positive byte count because an unbounded buffer can grow without limit behind
a wedged WAL write. To turn that pipeline off, set `internal_telemetry = false`.

#### The query deadline, and work that outlives a request

`timeout_secs` is one absolute deadline after authentication, including queue
waits, execution, and best-effort history writes. Request-body reading and response
delivery are outside it. Expiry before work starts is 503; after work starts it
is 504. Timed-out workers can retain permits until they finish. The fixed DSL
admission budgets cannot be raised through configuration.
See [capacity diagnosis and cancellation](/operate/health/#the-query-deadline-and-work-that-outlives-a-request).

#### `[server.rate_limit]`

Per-key rate limiting in requests per minute. Every API key gets an independent token bucket, sized per route class: `default_rpm` on the interactive API routes, `ingest_rpm` on `/api/v1/ingest`. Set a field to `0` to disable rate limiting for that route class.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `default_rpm` | integer | `100` | Requests/minute allowed per API key on the interactive API routes; `0` disables |
| `ingest_rpm` | integer | `1000` | Requests/minute allowed per ingest-permitted API key on `/api/v1/ingest`; `0` disables |

The shipper-sized ceiling is earned by the `ingest` permission, not by the route: a key without it stays on its `default_rpm` bucket when it posts to `/api/v1/ingest`, on top of being rejected with 403.

The two classes need ceilings orders of magnitude apart: vector flushes a batch per 1 MB / 5 s per source and sources commonly share one ingest key, while `/query`, `/export`, and `/stream` each run a DuckDB scan. Raise `ingest_rpm` for large shipper fleets; raise `default_rpm` for dashboard-heavy UI use.

##### Per-role `rate_rpm` override (ADR-0006 slice 1)

Class-of-service lives on the role row: `fleet-admin roles create --name shipper --perm trawl:ingest --rate-rpm 2000` gives every key holding that role a 2000-rpm ceiling. Precedence:

- A key's effective RPM is the **max `rate_rpm` across its roles** when any role sets it, and that value **overrides** the route-class config default — the two are never combined.
- The one effective number applies independently in each route class the key touches (a `rate_rpm = 2000` key gets a 2000-rpm interactive budget *and* a 2000-rpm ingest budget, spent separately).
- `rate_rpm` is always `> 0` (schema-enforced), so `0`-disables remains a config-default-only semantic; a role override on a key re-enables limiting even when the class default is `0`.
- Consequence: a shipper-sized `rate_rpm` also loosens that key's interactive routes. Operators who need the numbers to differ put the roles on different keys.

Re-tiering is in place and non-destructive: `fleet-admin roles set-rate shipper --rate-rpm 4000` changes the ceiling for every key holding the role (effective on the next request — roles are resolved per verify, never cached), and `fleet-admin roles set-rate shipper --default` clears the override so those keys fall back to the class defaults. Neither touches the role's permission bundle or its key assignments.

**Migration note**: the per-role config keys (`admin`, `analyst`, `reader`, `ingest`) were removed in ADR-0006 slice 0 — a config still carrying them fails validation at boot rather than being silently ignored. Replace `ingest` with `ingest_rpm`, and the interactive roles with `default_rpm`.

#### TLS auto-generation

When `tls_cert_path` and `tls_key_path` are omitted, trawld generates a self-signed ECDSA P-256 certificate at startup with SANs for `localhost`, `127.0.0.1`, and `::1`. The cert and key are written to `{state_dir}/tls/` (where `state_dir` is the parent of `data.path`). Configure client trust for the issuing certificate. `insecure = true` or `--insecure` bypasses verification and is suitable only for an explicitly selected local test connection.

The `tls_reload_interval_secs` setting polls the cert/key files for content changes and hot-reloads them without restarting the server.

#### Logging filter (`RUST_LOG`)

Unset or invalid `RUST_LOG` selects the packaged target filter; an invalid value
also emits one configuration warning. A valid value replaces the filter.
The daemon and browser proxy have different target names and defaults.
See [logging and authentication signals](/operate/health/#logging-filter-rust_log)
for both defaults and the events excluded from stored telemetry.

#### The query debug log

`server.query_log`, `TRAWL_QUERY_LOG`, or `--query-log` selects an owner-only
ndjson log containing identity, DSL, SQL parameters, source paths, and result
samples. `query_log_max_bytes` caps it with one `.1` generation; zero disables
rollover. Symlinks at the configured path are refused. Use a private directory.
See [enable, inspect, and remove the debug log](/operate/health/#the-query-debug-log).

### `[data]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `path` | string | *(required)* | Parquet data directory. Point it at a directory *inside* your storage volume, not at the mount point itself: a repin stages its shadow and set-aside generations as siblings (`data.repin-next/`, `data.repin-aside/`), and hardlinks and renames cannot cross a filesystem boundary. Both packaged layouts already do this (volume at `/var/lib/trawl`, data at `/var/lib/trawl/data`). Keep the whole corpus on that one filesystem, too — a volume mounted at an env/date/hour subtree (tiered storage) breaks the same hardlinks, and renaming an env directory that contains a mount point fails with `EBUSY`. A repin requested on a data root that *is* a mount point, or whose env subtree holds one, is refused before it builds anything, naming the offending path |

### `[auth]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `database_url` | string | *(required unless `FLEET_DATABASE_URL` is set)* | Fleet-auth postgres keystore URL — API keys and roles live here. The `FLEET_DATABASE_URL` environment variable takes precedence |
| `audit_interval_secs` | integer | `30` | Poll the fleet keystore for key *and role* changes from `fleet-admin` — emits `key_created` / `key_revoked` / `key_roles_changed` / `role_created` / `role_changed` / `role_deleted` audit events, each key event carrying the permissions its roles resolved to; `0` disables |

:::note
trawld will not start until the fleet database is reachable and migrated (`fleet-admin migrate`). Key revocation takes effect immediately — liveness is checked in postgres on every request. The bare `DATABASE_URL` override was removed in the slice-3 cutover (it is ceded to sqlx's test harness); a leftover `db_path` (the retired transitional SQLite store) fails validation with a message naming the migration. See the [fleet-auth cutover runbook](/reference/fleet-auth-cutover/) for migrating an existing deployment.
:::

### `[storage]`

Trawl's own app state — query history, saved queries, schedules, and report runs — lives in a **dedicated postgres database** owned by trawld (ADR-0004 slice 3).

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `database_url` | string | *(required unless `TRAWL_DATABASE_URL` is set)* | Trawl app-state postgres URL. The `TRAWL_DATABASE_URL` environment variable takes precedence. **No fallback to the `[auth]` URL** |

:::note
trawld migrates this database automatically at boot (it is the sole writer) and holds a session advisory lock for its lifetime — a second trawld against the same database fails startup instead of racing. Provision a separate database whose role owns the schema; see [database provisioning](/operate/deployment/#provision-the-databases) for the roles and ownership.
:::

### `[ingest]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `true` | Enable `POST /api/v1/ingest` endpoint |
| `max_body_bytes` | byte size | `"16M"` | Request body limit for ingest |
| `wal_dir` | path | `{data.path}/wal/` | Write-ahead log directory |
| `compaction_interval_secs` | integer | `10` | WAL → parquet compaction frequency |
| `daily_rollup` | bool | `true` | Merge hourly parquets into daily files |
| `internal_telemetry` | bool | `true` | Write server events as `service=trawld` |
| `event_bus_capacity` | integer | `4096` | Broadcast channel capacity for SSE |
| `hot_buffer_max_events` | integer | `100000` | Max events in hot buffer |
| `hot_buffer_max_bytes` | byte size | `"100M"` | Max hot buffer size (serialized) |
| `stats_interval_secs` | integer | `60` | Server stats telemetry interval; `0` disables |
| `telemetry_flush_interval_secs` | integer | `1` | Telemetry WAL flush interval |
| `telemetry_buffer_max_bytes` | byte size | `"16M"` | One memory budget for everything self-telemetry holds while the WAL is unhealthy — active buffer, retry queue and the in-flight batch (estimated charge, like `hot_buffer_max_bytes`). Must be positive; `0` is boot-fatal (use `internal_telemetry = false` to turn self-telemetry off). Enforced as events arrive: over budget the oldest queued batches are shed first, then the incoming event itself, counted in `trawl_telemetry_events_dropped_total{reason="buffer_cap"}` |
| `default_env` | string | `"prod"` | Fills a missing `env` on ingested events (repair code `env.defaulted`). Must pass the env charset and be a member of `envs` |
| `envs` | string list | `[default_env]` | Environment allowlist (ADR-0009). Events with an unlisted `env` hard-reject. Entries must match `[a-z0-9_-]{1,32}`; `wal` and `scheduled` are reserved. Validated at load — trawld refuses to start otherwise. The allowlist gates writes, not reads: removing an env stops new ingest but its directories stay queryable and age out normally |
| `trusted_relays` | CIDR list | `[]` | Peers (collectors/relays) whose address must never be stamped as an event's `host`: a host-less event from one of these is rejected instead of peer-repaired (HTTP) or kept with `host` omitted (syslog). Invalid entries are boot-fatal. Peer addresses are canonicalized before matching — a dual-stack bind's IPv4-mapped peer (`::ffff:10.1.2.3`) matches a plain v4 entry (`10.0.0.0/8`), and a mapped-form entry folds to its v4 meaning at load |
| `severity_from` | source list | `["severity", "severity_text", "level"]` | Wire keys `_severity` derives from, in precedence order — first *mappable* wins. Empty is legal and derives nothing |
| `time_from` | source list | `["_time", "timestamp", "@timestamp"]` | Wire keys `_time` derives from, in precedence order — first *present* wins. Must contain `_time` |

#### Derivation sources (`severity_from` / `time_from`)

`_severity` and `_time` are trawl-owned envelope slots, and these two lists are the whole of what they read. Derivation only **reads**: every source stays exactly where it arrived, as an ordinary queryable column under the name its sender chose, and severity derivation leaves sender values alone. Time repair is recorded when the proposed timestamp cannot be used.

An entry takes either spelling:

```toml
[ingest]
severity_from = ["severity", "severity_text", "level"]
time_from = ["_time", "timestamp", "@timestamp"]

```

Alternatively, declare a dialect for a numeric source:

```toml
[ingest]
severity_from = ["level", { field = "syslog_severity", dialect = "syslog" }]
```

`dialect` is `otel` (the default) or `syslog`, and it governs **numerics only** — a word like `error` always reads through the one token table, whichever dialect the entry declares. OTel numbers are 1–24 read straight; syslog numbers are 0–7 read inverted (`0` = emerg, `7` = debug). The ranges overlap, so no value-shape rule could tell the dialects apart: the operator asserting where a number came from is what licenses the inversion. That makes the typed form the **syslog-over-HTTP forwarder knob** — a collector that parses frames itself and ships the raw PRI numeral as its own field reaches exactly the inversion the native listener does, through the same mechanism rather than a privileged code path.

Precedence differs between the two lists on purpose. `severity_from` takes the first source that *maps* to something on the ladder, so an unmappable `level: "gold"` falls through to the next source. `time_from` takes the first source that is *present*, and an unparseable value there falls to arrival time (with the `time.from_ingest` repair) rather than reaching past itself — what `_time` holds is always explicable from one input. Only `_time` itself is consumed and replaced with its canonical form; every other source is left alone.

Both lists are validated at startup and **trawld refuses to start** on a bad entry, naming the list and the offending index — a source list that silently never matches would be the sharpest footgun in the design. The rules:

| Rule | Applies to |
|------|------------|
| Field name non-empty, at most 255 bytes | both |
| Already ASCII-lowercase (ingest folds every field name, so a `Level` entry could never match) | both |
| No duplicate field names within one list | both |
| At most 8 entries | both |
| No reserved (`_`-prefixed) names | `severity_from` |
| Must contain `_time`, which is also the *only* reserved name permitted | `time_from` |
| `dialect` must be `otel` or `syslog`; an unknown token names the vocabulary | `severity_from` |
| `dialect` on an entry is an error — dialects govern severity numerics alone | `time_from` |
| Empty list | legal for `severity_from` (derive nothing), illegal for `time_from` |

Changing either list is **forward-only**. There is no policy history and nothing re-derives stored events: an event keeps the `_severity` and `_time` it was written with, and the new lists apply from the next event onward. Neither list retroactively changes envelope fields. Repin can change custom pinned fields, but refuses declared envelope fields such as `_time` and `_severity`.

**Profile-fixed sources are not configurable.** Each producer profile prepends its own transport-proven sources to these lists: the syslog listener contributes `{ field = "syslog_severity", dialect = "syslog" }` and `syslog_timestamp`, while the HTTP and trawld doors contribute none. Those fixed entries win over anything configured under the same name and do not count against the 8-entry bound — they are trawl's, not the operator's. Everything else about the two lists is global and identical at every door.

### `[retention]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `max_age_days` | integer | `90` | Delete data older than N days in every env without an override; `0` keeps data forever for those envs only (an env with a finite override still ages out) |
| `min_free_disk_bytes` | byte size | `"1G"` | When free disk drops below, delete date directories highest expiry ratio first (age over that env's limit); `0` disables |
| `retention_interval_secs` | integer | `3600` | Retention check frequency (default: 1 hour) |

Repin markers or staging roots suppress both sweeps. Either epoch archive,
`data.pre-schema-v2/` or `data.pre-epoch-3/`, suppresses disk-pressure deletion
but not age retention. See [suppression and recovery](/operate/retention/#diagnose-suppression)
before removing any data or recovery state.

#### `[retention.env.<name>]`

One table per env gives that env its own age limit. The key is the env name
as it appears under the data root.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `max_age_days` | integer | *(required)* | Delete this env's data older than N days; `0` keeps it forever |

```toml
[retention]
max_age_days = 90
min_free_disk_bytes = "1G"
retention_interval_secs = 3600

[retention.env.prod]
max_age_days = 365

[retention.env.lab]
max_age_days = 7
```

An env with no table of its own keeps the global `max_age_days`. There is no
per-env disk floor and no per-env byte quota. One filesystem has one pool of
free space, so disk pressure stays install-wide.

`max_age_days` is required inside the table. An empty `[retention.env.lab]`
fails the load instead of picking between "inherit the global" and "keep
forever", which are opposite answers. The table also refuses unknown keys, so
a misplaced `min_free_disk_bytes` inside it fails the load rather than being
ignored. So does the whole `[retention]` section: `[retention.evn.prod]` is
refused by name at load instead of loading, being discarded, and leaving prod
on the global limit.

Keys are held to the env charset `[a-z0-9_-]{1,32}` with `wal` and `scheduled`
reserved, exactly as `ingest.envs` entries are, and a bad key is fatal at boot.
A key naming an env that is not in `ingest.envs` is legal. De-listing an env
stops new ingest for it while its directories stay on disk and still need an
age policy. An entry naming an env with no directory under the data root warns
at boot and keeps running (`retention_env_without_dir`). That is usually a
typo, and a typo here is otherwise silent, since the override governs nothing
and the env it was meant for keeps aging out under the global.

**Put the per-env tables last.** In TOML a sub-table header ends the table
above it, so a scalar written after `[retention.env.prod]` lands inside
`[retention.env.prod]`. `min_free_disk_bytes` written there is an unknown key
and fails the load. `max_age_days` written there is worse: if the table
already has one it is a duplicate key and fails, but if it does not, the
scalar you meant as the global becomes prod's own limit and loads clean,
with every other env on the default. Keep the three global scalars first and
the per-env tables after them.

##### How disk pressure ranks envs

Candidates rank by age divided by their env's effective age limit, largest first;
ties use older date then path. Zero-age-limit envs rank last but remain eligible.
See [the worked example](/operate/retention/#how-disk-pressure-ranks-envs).

##### The `/schema` horizon

The maximum global or per-env age is both the schema observation window and the
pin-GC floor. A zero anywhere disables that window; `?all=true` lifts it.
See [schema and retention](/operate/retention/#the-schema-horizon).

### `[scheduler]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `true` | Enable scheduled query execution |
| `poll_interval_secs` | integer | `10` | How often the scheduler looks for due schedules. It is the polling rate only: a schedule fires on its own planned boundary (`next_fire_at`), so a slow poll delays a run, it never shifts the window that run covers |
| `report_max_rows` | integer | `10000` | Max rows stored per report run |
| `max_runs_per_schedule` | integer | `100` | Completed runs kept per schedule |
| `report_retention_days` | integer | `30` | Delete report runs older than N days |
| `max_catchup_intervals` | integer | `24` | How many whole schedule intervals one `since_last` catch-up run may cover. Must be between 1 and 1000000; trawld refuses to start outside that range |

A schedule in `since_last` mode keeps a watermark and tiles forward from it, so runs missed while trawld was down do not vanish: the next successful run covers everything back to the watermark in one window. `max_catchup_intervals` bounds that window. Past the bound the start is clamped forward to `window_end - max_catchup_intervals * interval`, the run row carries `window_truncated: true`, and `trawl_scheduler_window_truncated_total` counts it. The coverage before the clamp is then permanently missing from the report series, which is the trade: one enormous query after a week of downtime would be worse. The unit is intervals, not hours, so the default of 24 means a day of missed hourly runs or 24 days of missed daily ones. Raise it if you would rather pay for the catch-up query than lose the coverage; there is no "never clamp" switch, spell that as a large number. The ceiling is 1000000, and trawld refuses to start above it: the scheduler multiplies this by the interval to get the widest window it may plan, and a number too large to multiply would stop every `since_last` schedule instead of un-clamping it. A million intervals is 114 years of missed hourly runs, so the bound costs nothing anyone means.

Window modes, `lag` and the watermark are per-schedule settings, not config: see [scheduled reports](/architecture/data-flow/#scheduled-reports) for the mechanism and the [schedules API](/reference/api/#schedules) for the request and response fields.

### `[web]`

Browser-facing session proxy (`trawl-web` binary). Reads the same `trawld.toml` and runs as a separate systemd unit (`trawl-web.service` on Debian, sidecar container in the Helm chart).

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `public_origins` | string array | *(required)* | Browser-visible origins allowed to carry a session cookie. Compared whole: scheme, host and port. An empty list is a startup error |
| `bind_addr` | string | `"127.0.0.1:8090"` | Listen address. Defaults to loopback — front with a reverse proxy for external access |
| `upstream_url` | string | derived from `[server].http_addr` | trawld URL. Wildcard binds are rewritten to loopback |
| `cookie_secret_path` | string | (none) | Path to a file holding the 32-byte AEAD cookie-encryption key |
| `cookie_secret_env` | string | (none) | Env var holding the base64-encoded key. Takes precedence over `cookie_secret_path` |
| `session_ttl_secs` | integer | `86400` | Browser session lifetime (24h default) |
| `allow_insecure_cookies` | bool | `false` | Drop `Secure` from session cookies. Set true only when the browser connects over HTTP. Keep false for browser HTTPS, including when a reverse proxy terminates TLS |
| `shared_domain` | string | (none) | Parent domain for the shared `fleet_session` SSO cookie, e.g. `".fleet.lab.ktle.net"`. Mirrors coastwatch's `session.shared_domain` — set the same value in both apps. Unset/empty → origin-scoped cookie (standalone mode) |

If neither `cookie_secret_path` nor `cookie_secret_env` is set, the proxy generates an ephemeral key on each startup. Sessions will not survive restart. The Debian `trawl-server` package creates a persistent key at `/var/lib/trawl/web.cookie` through its `postinst` script and preserves it on ordinary upgrades.

#### The browser-origin allowlist

`public_origins` is the CSRF control (ADR-0016). When a request carries an `Origin` header, `trawl-web` compares it whole against this list before it looks at the session cookie: scheme, host and port must all match. A request with no `Origin` passes, because this is a browser control and curl, the CLI and every scripted client send none.

State what the browser's address bar shows. A few consequences worth knowing before the first 403:

- **Spellings that reach the same server are still different origins.** `http://localhost:8090`, `http://127.0.0.1:8090` and `http://[::1]:8090` are three entries. So are `https://trawl.example.com` and `https://trawl.example.com.`, the trailing DNS root dot a browser keeps if you browsed to `https://trawl.example.com./`. The default port is the one thing that normalizes: `https://x` and `https://x:443` are one origin.
- **Behind a TLS-terminating proxy, configure the origin the browser sees**, e.g. `https://trawl.example.com`, not the `http://127.0.0.1:8090` the proxy forwards to. The backend never sees the browser's scheme, which is the whole reason the list is stated rather than derived.
- **`Forwarded` and `X-Forwarded-*` are never read**, from any peer. The verdict never depends on a header a proxy rewrites or a client can type, so no proxy configuration can widen the allowlist and none is needed to keep it working.
- **An empty list refuses to start.** There is no host-only fallback and no "empty means allow everything" default; both would fail silently, in opposite directions.
- A request carrying a sibling fleet app's origin is rejected unless that origin is in the allowlist, even when the apps share the `fleet_session` cookie. This blocks calls to protected endpoints, including logout. It does not protect the shared cookie from a compromised sibling: that app can overwrite or clear the parent-domain cookie through its own `Set-Cookie` response.

Setting `shared_domain` enables fleet-wide single sign-on: the session cookie is scoped to the parent domain and every fleet app under it accepts it, provided all apps share the same session key (see [shared browser sessions](/operate/access/#shared-browser-sessions) for key provisioning).

API clients using bearer tokens (the CLI, `trawl-client`, vector) talk to trawld directly on port 5514 — the proxy only handles cookie-authed browser traffic and blocks `/api/v1/ingest` outright.

```toml
[web]
public_origins = ["https://trawl.example.com"]
bind_addr = "127.0.0.1:8090"
cookie_secret_path = "/var/lib/trawl/web.cookie"
session_ttl_secs = 86400
```

The Debian package ships both loopback spellings of its own bind, since that is what a browser on the same host uses:

```toml
[web]
public_origins = ["http://127.0.0.1:8090", "http://localhost:8090"]
```

#### `trawl-web` environment variables

The proxy reads these at startup. Variables with a corresponding `[web]` field override that field, and the `FLEET_SESSION_*` variables log when they displace a configured value. `FLEET_SESSION_COOKIE_PATH` has no `[web]` counterpart.

| Variable | Description |
|----------|-------------|
| `FLEET_SESSION_PUBLIC_ORIGINS` | Comma-separated browser origins, e.g. `https://trawl.example.com,http://localhost:8090`. Replaces `[web] public_origins` outright; entries are parsed by the same rules, and a bad one is a startup error naming its index |
| `FLEET_SESSION_AEAD_KEY` | Base64 session AEAD key. Overrides `cookie_secret_path` / `cookie_secret_env` |
| `FLEET_SESSION_COOKIE_DOMAIN` | Cookie `Domain=`. An empty value means a host-only cookie |
| `FLEET_SESSION_COOKIE_SECURE` | `true` or `false`. Setting `false` clears `Secure` on the session cookie |
| `FLEET_SESSION_COOKIE_PATH` | Cookie `Path=`. The only accepted value is `/`; anything else fails startup rather than issuing a cookie at one scope and clearing it at another. There is no `[web]` counterpart, so this variable can only agree with the proxy or stop it |
| `TRAWL_WEB_BIND_ADDR` | Overrides `[web] bind_addr` |
| `TRAWL_WEB_INSECURE_UPSTREAM` | Skip TLS verification of the upstream trawld cert. Loopback only |

The Helm chart passes `FLEET_SESSION_PUBLIC_ORIGINS` to the sidecar as well as rendering `public_origins` into the generated TOML, so a `config.raw` that replaces that TOML still carries the allowlist.

### `[syslog]`

Native syslog listener for receiving logs from network appliances.

Frames enter the same envelope canonicalizer HTTP events do, under the `syslog` producer profile: the listener publishes what it parsed as ordinary columns — `syslog_severity` (the **raw** PRI numeral, 0–7, absent when the frame carried no PRI), `syslog_timestamp`, `syslog_facility`, `syslog_pid`, `syslog_msgid`, `syslog_source_ip` and the flattened `sd_*` structured-data pairs — and the profile's fixed derivation sources turn the first two into `_severity` and `_time`. `env` comes from `[ingest] default_env` and `host` from the frame's hostname, falling back to the peer address unless the peer is a configured `[ingest] trusted_relays` CIDR, in which case the event is kept with `host` absent. An APP-NAME that fails the service charset lands under `default_service` with a `service.from_profile` repair rather than being sanitized into a service name nobody sent.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Enable syslog listener |
| `udp_addr` | string | `"0.0.0.0:1514"` | UDP listen address |
| `udp_enabled` | bool | `true` | Enable UDP listener |
| `tcp_addr` | string | `"0.0.0.0:1514"` | TCP listen address |
| `tcp_enabled` | bool | `true` | Enable TCP listener |
| `max_tcp_connections` | integer | `256` | Max concurrent TCP connections |
| `batch_interval_ms` | integer | `500` | Batch flush interval (milliseconds) |
| `batch_max_events` | integer | `1000` | Max events per batch before flush |
| `default_service` | string | `"syslog"` | Fallback service name |
| `tcp_idle_timeout_secs` | integer | `60` | Close idle TCP connections after N seconds |
| `max_events_per_connection` | integer | `100000` | Per-connection event limit |
| `consecutive_send_failures_limit` | integer | `100` | Close connection after N failed sends |
| `channel_capacity` | integer | `10000` | Event queue between listeners and batcher |
| `allow_cidrs` | string array | `[]` | Source IP allowlist in CIDR notation. Peer addresses canonicalize before matching (an IPv4-mapped peer matches a plain v4 entry); write v4 intent in v4 form. A v6 entry wide enough to cover the whole mapped range (`::/0`, `::ffff:0:0/95`) admits **all** v4 peers on every bind — it always did on a dual-stack bind, and the rule no longer depends on how the socket was bound |
| `source_service_map` | map | `{}` | Source IP → service name mapping. IP-shaped keys canonicalize at load, so a mapped spelling (`"::ffff:10.1.2.3"`) and its v4 form are one key — two spellings naming *different* services refuse to start |

```toml
[syslog]
enabled = true
udp_addr = "0.0.0.0:1514"
tcp_addr = "0.0.0.0:1514"
allow_cidrs = ["192.168.0.0/16", "10.0.0.0/8"]
default_service = "syslog"

[syslog.source_service_map]
"192.168.1.1" = "firewall"
"192.168.1.10" = "unifi"
```

### Example server config

```toml
[server]
http_addr = "0.0.0.0:5514"
timeout_secs = 30
tls_cert_path = "/etc/trawl/tls/cert.pem"
tls_key_path = "/etc/trawl/tls/key.pem"

[server.rate_limit]
default_rpm = 100
ingest_rpm = 1000

[data]
path = "/var/lib/trawl/data"

[auth]
database_url = "postgres://fleet:CHANGE_ME@db.internal:5432/fleet"

[storage]
database_url = "postgres://trawl:CHANGE_ME@db.internal:5432/trawl"

[ingest]
enabled = true
max_body_bytes = "16M"
internal_telemetry = true

[retention]
max_age_days = 90
min_free_disk_bytes = "1G"

# Sub-tables go last: a scalar after this header lands inside it.
[retention.env.prod]
max_age_days = 365
```

---

## Client configuration

The trawl CLI and TUI are configured via `~/.config/trawl/config.toml`.

### `[server]`

Default server connection settings (used when no `--profile` is specified).

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `url` | string | `"https://localhost:5514"` | Server URL |
| `token` | string | *(none)* | API token |
| `insecure` | bool | `false` | Accept self-signed TLS certificates |

### `[profiles.*]`

Named profiles for multiple servers. Each profile can override `url`, `token`, and `insecure`.

```toml
[server]
url = "https://trawl.prod.example.com:5514"
token = "flt_prod_token"

[profiles.dev]
url = "https://localhost:5514"
token = "flt_dev_token"
insecure = true

[profiles.staging]
url = "https://trawl.staging.example.com:5514"
token = "flt_staging_token"
```

Select a profile with `--profile dev`, `-p dev`, or `TRAWL_PROFILE=dev`.

### `[ui]`

TUI display settings.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `theme` | string | `"default"` | Theme name or path to custom theme |
| `enable_mouse` | bool | `true` | Mouse support in TUI |
| `auto_save_history` | bool | `true` | Auto-save query history |
| `tab_width` | integer | `2` | Editor tab width (spaces) |
| `timezone` | string | `"local"` | Timestamp display: `"local"`, `"UTC"`, or `"+HH:MM"` |

### `[tail]`

Live tail settings.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `max_events` | integer | `1000` | Live tail buffer size |

### Environment variables

CLI flags and environment variables override config file settings.

| Variable | Description |
|----------|-------------|
| `TRAWL_PROFILE` | Named profile to use |
| `TRAWL_URL` | Server URL |
| `TRAWL_TOKEN` | API token |
| `TRAWL_INSECURE` | Accept self-signed certs (`true`/`1`) |
