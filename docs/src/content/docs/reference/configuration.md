---
title: Configuration
description: Every trawld and trawl client configuration key, with its type, default, and effect.
---

## Server configuration

`trawld` reads one TOML file, at `~/.trawl/trawld.toml` by default.

```bash
trawld --config /etc/trawl/trawld.toml
```

### Validate without starting the daemon

```bash
trawld --check-config --config /etc/trawl/trawld.toml
```

Check mode requires an explicit `--config` path or `TRAWL_CONFIG`. It checks
TOML syntax, supported setting names and types, config constraints, ingest
derivation settings, and the presence of both database URLs. It exits with
code 0 for valid configuration or a nonzero code for an error.

Check mode does not connect to databases, bind listeners, generate keys or
certificates, start crash capture, or write data and log files. It does not
verify database credentials, network reachability, certificate contents, or
filesystem permissions. Environment overrides apply as they do at startup.

All typed sections reject unknown settings, including nested sections.
Dynamic tables such as `retention.env.<name>` and `syslog.source_service_map`
accept operator-defined names; each retention entry still requires supported
fields. Error messages identify the setting path without printing its value
or the surrounding configuration text.

### Server environment variables

| Variable | Description |
|----------|-------------|
| `TRAWL_CONFIG` | Config file path. Same as `--config` |
| `TRAWL_QUERY_LOG` | ndjson query debug log path. Overrides `[server] query_log` |
| `TRAWL_HTTP_ADDR` | HTTPS listen address. Overrides `[server] http_addr` |
| `FLEET_DATABASE_URL` | Fleet keystore postgres URL. Overrides `[auth] database_url` |
| `TRAWL_DATABASE_URL` | Trawl app-state postgres URL. Overrides `[storage] database_url` |
| `RUST_LOG` | Tracing filter. An unset or invalid value selects the packaged filter, and an invalid value also logs one warning. See [logging and authentication signals](/operate/health/#restore-missing-log-lines) |

`trawld` does not read `DATABASE_URL`. That variable belongs to `fleet-admin`.

### Value syntax

A byte size accepts an integer count of bytes, or a string with a 1024-based
suffix: `B`, `K`, `M`, `G`, or `T`. The `KB`, `KIB`, and lowercase spellings
mean the same thing, and a decimal such as `"1.5G"` is legal.

```toml
max_body_bytes = "16M"   # 16 * 1024 * 1024 = 16777216
```

A path expands a leading `~/` to `$HOME/` at startup.

### `[server]`

The HTTPS listener, query limits, TLS, and logging.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `http_addr` | string | `"127.0.0.1:5514"` | HTTPS listen address |
| `timeout_secs` | integer | `30` | Absolute query deadline in seconds, counted from authentication and including queue waits |
| `max_concurrent_queries` | integer | *(CPU count)* | DuckDB executor pool size. Must be greater than 0 |
| `max_result_rows` | integer | `100000` | Rows a query may return before trawld rejects it |
| `max_export_rows` | integer | `1000000` | Rows an export may return. Exports do not use `max_result_rows` |
| `max_request_body_bytes` | byte size | `"128K"` | Request body limit on every route except ingest |
| `max_concurrent_requests` | integer | `256` | Concurrent HTTP requests. Past it trawld answers 503 |
| `shutdown_drain_secs` | integer | `30` | Graceful shutdown budget for in-flight requests |
| `log_file` | path | *(none)* | JSON log file. Opened when `[ingest] enabled` or `internal_telemetry` is false. When both are true, server events use the ingest pipeline and this path is not opened. File logging starts after database and storage admission; earlier JSON events go to stderr. Must not name or alias the data root's `EPOCH`, `CATALOG`, or `REPIN` marker |
| `tls_cert_path` | path | *(generated)* | PEM certificate |
| `tls_key_path` | path | *(generated)* | PEM private key |
| `tls_reload_interval_secs` | integer | `300` | How often trawld polls the certificate files for changes. `0` disables reloading |
| `query_log` | path | *(none)* | ndjson debug log, one entry per query execution. See [the query debug log](#the-query-debug-log) |
| `query_log_max_bytes` | byte size | `"100M"` | Size cap for the query debug log, with one retained `<path>.1` generation. `0` disables rollover |
| `cors_allowed_origins` | string array | `[]` | CORS origins. An empty list sends no CORS headers |
| `schema_cache_ttl_secs` | integer | `60` | Cache lifetime for unscoped `GET /schema` reads, `/schema/values` samples, and the background schema refresh. A `?service=` read is always fresh |
| `max_query_history` | integer | `1000` | Completed queries kept in the in-memory ring buffer |
| `max_sse_connections` | integer | `32` | Concurrent SSE streams. Past it trawld answers 429 |
| `monitor_refresh_ms` | integer | `1000` | Monitor dashboard refresh interval. Used only when trawld runs on a TTY |

Notes:

- `tls_cert_path` and `tls_key_path` must both be set or both omitted. With both omitted, trawld generates a self-signed ECDSA P-256 certificate at startup, with SANs for `localhost`, `127.0.0.1`, and `::1`, under `{state_dir}/tls/`. `state_dir` is the parent of `[data] path`.
- `timeout_secs` covers queue waits, execution, and best-effort history writes, but not request-body reading or response delivery. Expiry before work starts is 503, and after work starts it is 504. A timed-out worker can hold its permit until it finishes. See [the query deadline](/operate/health/#diagnose-a-503-or-504-from-a-query).
- `0` disables a limit only where the row says so.

#### `[server.rate_limit]`

Every API key gets an independent token bucket per route class.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `default_rpm` | integer | `100` | Requests per minute per key on the interactive API routes. `0` disables |
| `ingest_rpm` | integer | `1000` | Requests per minute per key on `/api/v1/ingest`. `0` disables |

```toml
[server.rate_limit]
default_rpm = 100
ingest_rpm = 1000
```

Notes:

- The `ingest_rpm` ceiling is earned by the `ingest` permission. A key without it draws on its `default_rpm` bucket when it posts to `/api/v1/ingest`, and the route answers 403.
- A role's `rate_rpm` in the fleet keystore replaces the class default for every key holding that role, and the largest `rate_rpm` across a key's roles wins. That one value applies in each route class the key touches, spent separately.
- `rate_rpm` is always greater than 0, so a role override re-enables limiting for its keys even when the class default is `0`.

#### The query debug log

`[server] query_log`, `TRAWL_QUERY_LOG`, or `--query-log` selects an owner-only
ndjson log. Each entry combines identity, raw query text, SQL parameter values,
source paths, and result samples, so keep the file in a private directory.
trawld refuses a symlink at the configured path. See
[enable, inspect, and remove the debug log](/operate/health/#enable-the-query-debug-log).

### Helm TLS selection

The chart's `tls.mode` selects `auto` for a daemon-generated self-signed
certificate, `secret` for the existing `tls.secretName`, or `certManager` to
create a Certificate and mount its generated Secret. cert-manager mode requires
`tls.certManager.issuerRef.name` and `tls.certManager.dnsNames`; issuer kind
defaults to `ClusterIssuer` and group to `cert-manager.io`. Namespaced `Issuer`
resources must be in the release namespace.

Both Secret modes mount `tls.crt` and `tls.key` at `/etc/trawl/tls/` and set the
daemon paths accordingly. They require structured config values;
`config.raw` is supported only with `tls.mode: auto`, without chart-managed
TLS Secret mounts. See [configure the daemon API certificate](/operate/deployment/#configure-the-daemon-api-certificate)
for complete setup and verification instructions. Browser-ingress TLS remains
a separate setting under `ingress.tls`.

### `[data]`

The daemon owns a directory tree. For local file or glob selection, use the
CLI's `trawl query --data` option instead.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `path` | string | *(required)* | Parquet data directory. Glob metacharacters (`*`, `?`, `[`) are not accepted |

Notes:

- Point `path` at a directory inside the storage volume, not at the mount point. A repin stages `data.repin-next/` and `data.repin-aside/` as siblings, and hardlinks and renames cannot cross a filesystem boundary. Both packaged layouts put the volume at `/var/lib/trawl` and the data at `/var/lib/trawl/data`.
- Keep the whole corpus on one filesystem. A repin is refused before it builds anything when the data root is a mount point or its env subtree holds one, and the refusal names the path.

```toml
[data]
path = "/var/lib/trawl/data"
```

### `[auth]`

API keys and roles live in the fleet-auth postgres keystore.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `database_url` | string | *(required unless `FLEET_DATABASE_URL` is set)* | Fleet keystore URL. `FLEET_DATABASE_URL` takes precedence |
| `audit_interval_secs` | integer | `30` | How often trawld polls the keystore for key and role changes made by `fleet-admin`. `0` disables polling |

```toml
[auth]
database_url = "postgres://fleet:CHANGE_ME@db.example.com:5432/fleet"
```

Notes:

- trawld starts only after the fleet database is reachable and initialized with `fleet-admin migrate`. See [database provisioning](/operate/deployment/#provision-the-databases).
- trawld checks key validity on every request, so a revocation takes effect on the next one.
- Polling emits `key_created`, `key_revoked`, `key_roles_changed`, `role_created`, `role_changed`, and `role_deleted` audit events. Each key event carries the permissions its roles resolved to.

### `[ingest]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `true` | Enable `POST /api/v1/ingest` |
| `max_body_bytes` | byte size | `"16M"` | Request body limit for ingest |
| `wal_dir` | path | `{data.path}/wal/` | Write-ahead log directory |
| `compaction_interval_secs` | integer | `10` | How often the WAL-to-parquet compaction task runs |
| `internal_telemetry` | bool | `true` | Write server events into the ingest pipeline as `service=trawld` |
| `daily_rollup` | bool | `true` | Merge hourly parquet files into daily files for older dates |
| `event_bus_capacity` | integer | `4096` | Broadcast channel capacity behind SSE. A full channel makes slow subscribers lag |
| `hot_buffer_max_events` | integer | `100000` | Events held in the hot buffer. Oldest batches are evicted first |
| `hot_buffer_max_bytes` | byte size | `"100M"` | Estimated memory budget for the hot buffer |
| `stats_interval_secs` | integer | `60` | How often server stats are emitted as telemetry. `0` disables |
| `telemetry_flush_interval_secs` | integer | `1` | How often buffered tracing events are flushed to the WAL |
| `telemetry_buffer_max_bytes` | byte size | `"16M"` | One estimated memory budget for everything self-telemetry holds while the WAL is unhealthy: active buffer, retry queue, and the batch in flight. Minimum `"64K"` |
| `compaction_chunk_size` | integer | `500` | Maximum WAL files merged per compaction chunk. A larger backlog is split into chunks |
| `compaction_memory_limit` | string | `"2GB"` | DuckDB memory limit for compaction connections, as a DuckDB memory string |
| `default_env` | string | `"prod"` | Fills a missing `env`, recorded as the `env.defaulted` repair. Must pass the env charset and belong to `envs` |
| `envs` | string array | `[default_env]` | Environment allowlist. Entries must match `[a-z0-9_-]{1,32}`. `wal` and `scheduled` are reserved |
| `trusted_relays` | CIDR array | `[]` | Peers whose address trawld must never stamp as a `host` |
| `severity_from` | source array | `["severity", "severity_text", "level"]` | Wire keys `_severity` derives from, in precedence order |
| `time_from` | source array | `["_time", "timestamp", "@timestamp"]` | Wire keys `_time` derives from, in precedence order. Must contain `_time` |

```toml
[ingest]
enabled = true
max_body_bytes = "16M"
default_env = "prod"
envs = ["prod", "lab"]
```

Notes:

- `telemetry_buffer_max_bytes` must be at least `64K` (65536 bytes), and a smaller value, including `0`, fails the load. The floor leaves room for the `telemetry_dropped` record that reports buffer drops. To turn self-telemetry off, set `internal_telemetry = false`. Over the budget, trawld sheds the oldest queued batches first and then the incoming event, counted in `trawl_telemetry_events_dropped_total{reason="buffer_cap"}`.
- An event whose `env` is not in the allowlist is rejected. The allowlist gates writes only: removing an env stops new ingest for it while its directories stay queryable and age out normally. trawld validates the list at load and refuses to start on a bad entry.
- A host-less event from a `trusted_relays` peer is rejected on HTTP, and kept with `host` omitted on syslog. An invalid CIDR entry is fatal at boot. Peer addresses canonicalize before matching, so an IPv4-mapped peer such as `::ffff:192.0.2.7` matches a plain v4 entry.

#### Derivation sources (`severity_from` / `time_from`)

Derivation only reads. Every source stays where it arrived, as an ordinary
queryable column under its sender's name. These are the defaults, and the
packaged `trawld.toml` and the Helm chart state the same lists:

```toml
[ingest]
severity_from = ["severity", "severity_text", "level"]
time_from = ["_time", "timestamp", "@timestamp"]
```

An entry takes the bare spelling or the typed spelling. For a sender whose
`syslog_severity` carries syslog numerals:

```toml
[ingest]
severity_from = ["level", { field = "syslog_severity", dialect = "syslog" }]
time_from = ["_time", "timestamp", "@timestamp"]
```

`dialect` is `otel` or `syslog`, and it governs numerals only. A word such as
`error` always reads through the one token table. OTel numerals are 1 to 24 read
straight. Syslog numerals are 0 to 7 read inverted, where `0` is emerg and `7`
is debug.

trawld validates both lists at startup and refuses to start on a bad entry,
naming the list and the index.

| Rule | Applies to |
|------|------------|
| Field name non-empty, at most 255 bytes | both |
| Already ASCII-lowercase | both |
| No duplicate field names within one list | both |
| At most 8 entries | both |
| No reserved (`_`-prefixed) names | `severity_from` |
| Must contain `_time`, the only reserved name permitted | `time_from` |
| `dialect` must be `otel` or `syslog` | `severity_from` |
| `dialect` on an entry is an error | `time_from` |
| Empty list is legal and derives nothing | `severity_from` |

Each producer profile prepends its own fixed sources. Those are not
configurable, they beat a configured entry of the same name, and they do not
count against the 8-entry bound. A change to either list applies from the next
event onward and re-derives no stored event. For the derivation rules, see
[the event contract](/reference/events/#severity-derivation).

### `[retention]`

Two policies run independently. Age retention deletes a date directory once it
is older than the limit that applies to its env. Disk-pressure retention is
install-wide.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `max_age_days` | integer | `90` | Delete data older than this many days in every env without an override. `0` keeps that data forever |
| `min_free_disk_bytes` | byte size | `"1G"` | Below this much free disk, delete date directories until free space is back above it. `0` disables |
| `retention_interval_secs` | integer | `3600` | How often the retention task runs |

```toml
[retention]
max_age_days = 90
min_free_disk_bytes = "1G"
retention_interval_secs = 3600

# Sub-tables go last: a scalar after this header lands inside it.
[retention.env.prod]
max_age_days = 365
```

Notes:

- Disk-pressure candidates rank by age divided by the env's effective age limit, largest first, then older date, then path. An env with a zero age limit ranks last and stays eligible. See [free disk space](/operate/retention/#free-disk-space).
- The largest global or per-env age is both the `/schema` observation window and the pin-GC floor. A zero anywhere disables that window, and `?all=true` lifts it.
- A repin marker or a staging root suppresses both sweeps. See [clear suppressed retention](/operate/retention/#clear-suppressed-retention).

#### `[retention.env.<name>]`

One table per env gives that env its own age limit. The key is the env name as
it appears under the data root.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `max_age_days` | integer | *(required)* | Delete this env's data older than this many days. `0` keeps it forever |

Notes:

- `[retention]` and each `[retention.env.<name>]` table refuse keys they do not know, so `[retention.evn.prod]` fails the load. `max_age_days` is required inside the table, so an empty `[retention.env.lab]` fails the load too.
- Keys use the env charset `[a-z0-9_-]{1,32}` with `wal` and `scheduled` reserved, and a bad key is fatal at boot.
- A key naming an env that is not in `ingest.envs` is legal. A key naming an env with no directory under the data root warns at boot as `retention_env_without_dir`, and trawld keeps running.
- Write the three global scalars before the per-env tables. In TOML a sub-table header ends the table above it, so a `max_age_days` written after `[retention.env.prod]` becomes prod's limit and loads clean.
- There is no per-env disk floor and no per-env byte quota.

### `[scheduler]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `true` | Enable scheduled query execution |
| `poll_interval_secs` | integer | `10` | How often the scheduler looks for due schedules |
| `report_max_rows` | integer | `10000` | Rows stored per report run |
| `max_runs_per_schedule` | integer | `100` | Completed runs kept per schedule |
| `report_retention_days` | integer | `30` | Delete report runs older than this many days |
| `max_catchup_intervals` | integer | `24` | Whole schedule intervals one `since_last` catch-up run may cover. Must be between 1 and 1000000 |

```toml
[scheduler]
enabled = true
poll_interval_secs = 10
max_catchup_intervals = 24
```

Notes:

- `poll_interval_secs` is the polling rate only. A schedule fires on its planned boundary, so a slow poll delays a run without shifting the window that run covers.
- Runs missed while trawld was down coalesce into one window. Past `max_catchup_intervals`, the start clamps forward to `window_end - max_catchup_intervals * interval`, the run row carries `window_truncated: true`, and `trawl_scheduler_window_truncated_total` counts it. A manual run of a `since_last` schedule is clamped and counted the same way. The coverage before the clamp is then missing from the report series.
- The unit is intervals, not hours, so the default of 24 covers a day of missed hourly runs or 24 days of missed daily ones. There is no switch for "never clamp": spell it as a large number.
- Window modes, `lag`, and the watermark are per-schedule settings. See [scheduled reports](/architecture/reports-telemetry/#scheduled-reports) and the [schedules API](/reference/api/#schedules).

### `[syslog]`

The native syslog listener receives frames from network appliances. Its field
mapping is in [the event contract](/reference/events/#syslog-listener-fields).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Enable the syslog listener |
| `udp_addr` | string | `"0.0.0.0:1514"` | UDP listen address |
| `udp_enabled` | bool | `true` | Enable the UDP listener |
| `tcp_addr` | string | `"0.0.0.0:1514"` | TCP listen address |
| `tcp_enabled` | bool | `true` | Enable the TCP listener |
| `max_tcp_connections` | integer | `256` | Concurrent TCP connections |
| `batch_interval_ms` | integer | `500` | Batch flush interval in milliseconds |
| `batch_max_events` | integer | `1000` | Events per batch before a forced flush |
| `default_service` | string | `"syslog"` | Service name used when no APP-NAME and no source-IP mapping applies |
| `tcp_idle_timeout_secs` | integer | `60` | Close a TCP connection that sends no data for this long |
| `max_events_per_connection` | integer | `100000` | Events accepted from one TCP connection before it is closed |
| `consecutive_send_failures_limit` | integer | `100` | Close a connection after this many consecutive failed sends to the batcher |
| `allow_cidrs` | string array | `[]` | Source IP allowlist in CIDR notation. An empty list accepts every source IP |
| `source_service_map` | map | `{}` | Source IP to service name. Takes priority over the frame's APP-NAME |
| `channel_capacity` | integer | `10000` | Event queue capacity between the listeners and the batcher |

```toml
[syslog]
enabled = true
udp_addr = "0.0.0.0:1514"
tcp_addr = "0.0.0.0:1514"
allow_cidrs = ["192.0.2.0/24", "198.51.100.0/24"]
default_service = "syslog"

[syslog.source_service_map]
"192.0.2.1" = "firewall"
"192.0.2.10" = "unifi"
```

Notes:

- A UDP source IP can be spoofed on the local network, so `allow_cidrs` is not authentication for UDP traffic.
- A bare IP in `allow_cidrs` counts as `/32` for IPv4 and `/128` for IPv6. Peer addresses canonicalize before matching, so write v4 intent in v4 form. A v6 entry wide enough to cover the mapped range, such as `::/0` or `::ffff:0:0/95`, admits every v4 peer.
- IP-shaped keys in `source_service_map` canonicalize at load, so a mapped spelling and its v4 form are one key, and two spellings naming different services refuse to start.
- The listener needs `[ingest] enabled = true`. With ingest off, trawld logs a warning and disables syslog.

### `[web]`

The browser-facing session proxy `trawl-web` reads the same `trawld.toml` and
runs as its own service: `trawl-web.service` on Debian, a sidecar container in
the Helm chart.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `bind_addr` | string | `"127.0.0.1:8090"` | Proxy listen address. Front it with a reverse proxy for external access |
| `upstream_url` | string | derived from `[server] http_addr` | How the proxy reaches trawld. A wildcard bind is rewritten to loopback |
| `cookie_secret_path` | path | *(none)* | File holding the 32-byte AEAD cookie-encryption key |
| `cookie_secret_env` | string | *(none)* | Name of an environment variable holding the base64-encoded key |
| `session_ttl_secs` | integer | `86400` | Browser session lifetime in seconds |
| `allow_insecure_cookies` | bool | `false` | Drop `Secure` from session cookies. Set it true only when the browser connects over HTTP |
| `public_origins` | string array | *(required)* | Browser-visible origins allowed to carry a session cookie. An empty list is a startup error |
| `shared_domain` | string | *(none)* | Parent domain for the shared `fleet_session` SSO cookie, such as `".example.com"`. Unset or empty means an origin-scoped cookie |

```toml
[web]
public_origins = ["https://trawl.example.com"]
bind_addr = "127.0.0.1:8090"
cookie_secret_path = "/var/lib/trawl/web.cookie"
session_ttl_secs = 86400
```

Notes:

- With neither `cookie_secret_path` nor `cookie_secret_env` set, the proxy generates an ephemeral key at each startup, and sessions do not survive a restart. The Debian `trawl-server` package creates a persistent key at `/var/lib/trawl/web.cookie` and preserves it on upgrade.
- `shared_domain` scopes the session cookie to a parent domain, so every Fleet app under it accepts one login. All those apps need the same session key. See [shared browser sessions](/operate/access/#share-a-browser-session-across-fleet-applications).
- API clients that carry a bearer token talk to trawld directly. The proxy handles cookie-authenticated browser traffic only, and blocks `/api/v1/ingest`.

#### The browser-origin allowlist

`public_origins` is the CSRF control. When a request carries an `Origin` header,
`trawl-web` compares it whole against this list before reading the session
cookie. A request with no `Origin` passes, because scripted clients send none.
State the origin the browser's address bar shows.

- Scheme, host, and port must all match. The default port is the one thing that normalizes, so `https://x` and `https://x:443` are one origin.
- Spellings that reach the same server are still separate origins. List `http://localhost:8090`, `http://127.0.0.1:8090`, and `http://[::1]:8090` separately. A trailing DNS root dot, as in `https://trawl.example.com.`, is a fourth.
- Behind a TLS-terminating proxy, list the origin the browser sees, not the address the proxy forwards to.
- `Forwarded` and `X-Forwarded-*` are never read, from any peer.
- An empty list refuses to start. There is no host-only fallback and no "empty allows everything" default.
- A sibling Fleet app's origin is rejected unless it is listed, even when both apps share the `fleet_session` cookie. That blocks calls to protected endpoints, including logout. It does not protect the shared cookie from a compromised sibling, which can overwrite or clear it through its own `Set-Cookie` response.

The Debian package lists both loopback spellings of its own bind:

```toml
[web]
public_origins = ["http://127.0.0.1:8090", "http://localhost:8090"]
```

#### `trawl-web` environment variables

A variable with a matching `[web]` key overrides that key. The `FLEET_SESSION_*`
variables log a line when they displace a configured value.

| Variable | Description |
|----------|-------------|
| `FLEET_SESSION_PUBLIC_ORIGINS` | Comma-separated browser origins. Replaces `[web] public_origins` outright. A bad entry is a startup error naming its index |
| `FLEET_SESSION_AEAD_KEY` | Base64 session AEAD key. Overrides `cookie_secret_path` and `cookie_secret_env` |
| `FLEET_SESSION_COOKIE_DOMAIN` | Cookie `Domain=`. An empty value means a host-only cookie |
| `FLEET_SESSION_COOKIE_SECURE` | `true` or `false`. `false` clears `Secure` on the session cookie |
| `FLEET_SESSION_COOKIE_PATH` | Cookie `Path=`. The only accepted value is `/`. There is no `[web]` counterpart |
| `TRAWL_WEB_BIND_ADDR` | Overrides `[web] bind_addr` |
| `TRAWL_WEB_INSECURE_UPSTREAM` | Skip TLS verification of the upstream trawld certificate. Loopback only |

The Helm chart passes `FLEET_SESSION_PUBLIC_ORIGINS` to the sidecar as well as
rendering `public_origins` into the generated TOML, so a `config.raw` that
replaces that TOML still carries the allowlist.

### `[storage]`

Query history, saved queries, schedules, and report runs live in a dedicated
`trawl` postgres database.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `database_url` | string | *(required unless `TRAWL_DATABASE_URL` is set)* | App-state postgres URL. `TRAWL_DATABASE_URL` takes precedence. There is no fallback to the `[auth]` URL |

```toml
[storage]
database_url = "postgres://trawl:CHANGE_ME@db.example.com:5432/trawl"
```

Notes:

- trawld migrates this database at boot and holds a session advisory lock for its lifetime, so a second trawld against the same database fails to start.
- Provision a separate database whose role owns the schema. See [database provisioning](/operate/deployment/#provision-the-databases).

### A minimal server config

Only `[data] path`, a fleet keystore URL, an app-state database URL, and
`[web] public_origins` have no working default.

```toml
[server]
http_addr = "0.0.0.0:5514"

[data]
path = "/var/lib/trawl/data"

[auth]
database_url = "postgres://fleet:CHANGE_ME@db.example.com:5432/fleet"

[storage]
database_url = "postgres://trawl:CHANGE_ME@db.example.com:5432/trawl"

[web]
public_origins = ["https://trawl.example.com"]
cookie_secret_path = "/var/lib/trawl/web.cookie"
```

---

## Client configuration

The `trawl` CLI and TUI read `~/.config/trawl/config.toml`. A flag beats an
environment variable, which beats the config file. See the
[CLI reference](/reference/cli/#global-options) for the flags.

### `[server]`

The default connection, used when no `--profile` is selected.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `url` | string | `"https://localhost:5514"` | Server URL |
| `token` | string | *(none)* | API token |
| `insecure` | bool | `false` | Accept self-signed TLS certificates. `trawl` then prints a warning to stderr |
| `ca_cert` | path | *(none)* | PEM file of the CA certificates to trust for this server. `trawl` trusts only these CAs and still checks the hostname. The path must be absolute or start with `~`. Setting it together with `insecure` is an error |

### `[profiles.<name>]`

One table per named server. Each key overrides the matching `[server]` key.
Select a profile with `-p dev`, `--profile dev`, or `TRAWL_PROFILE=dev`.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `url` | string | *(inherits `[server]`)* | Server URL |
| `token` | string | *(inherits `[server]`)* | API token |
| `insecure` | bool | *(inherits `[server]`)* | Accept self-signed TLS certificates |
| `ca_cert` | path | *(inherits `[server]`)* | PEM file of the CA certificates to trust. `""` clears an inherited `ca_cert` |

```toml
[server]
url = "https://trawl.example.com:5514"
token = "flt_prod_token"

[profiles.dev]
url = "https://localhost:5514"
token = "flt_dev_token"
ca_cert = "~/.config/trawl/dev-ca.pem"
```

### `[ui]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `theme` | string | `"default"` | Theme name, or a path to a theme file |
| `enable_mouse` | bool | `true` | Mouse support in the TUI |
| `auto_save_history` | bool | `true` | Save query history automatically |
| `tab_width` | integer | `2` | Editor tab width in spaces |
| `timezone` | string | `"local"` | Timestamp display: `"local"`, `"UTC"`, or a fixed offset such as `"+05:30"` |

```toml
[ui]
timezone = "UTC"
```

### `[tail]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `max_events` | integer | `1000` | Live tail buffer size in events |

```toml
[tail]
max_events = 1000
```
