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
| `timeout_secs` | integer | `30` | Query execution timeout (seconds) |
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
| `query_log` | path | *(none)* | ndjson debug log (one entry per query execution) |
| `log_file` | path | *(none)* | JSON log file; superseded by `internal_telemetry` |

#### `[server.rate_limit]`

Per-key rate limiting in requests per minute. Every API key gets an independent token bucket, sized per route class: `default_rpm` on the interactive API routes, `ingest_rpm` on `/api/v1/ingest`. Set a field to `0` to disable rate limiting for that route class.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `default_rpm` | integer | `100` | Requests/minute allowed per API key on the interactive API routes; `0` disables |
| `ingest_rpm` | integer | `1000` | Requests/minute allowed per ingest-permitted API key on `/api/v1/ingest`; `0` disables |

The shipper-sized ceiling is earned by the `ingest` permission, not by the route: a key without it stays on its `default_rpm` bucket when it posts to `/api/v1/ingest`, on top of being rejected with 401.

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

When `tls_cert_path` and `tls_key_path` are omitted, trawld generates a self-signed ECDSA P-256 certificate at startup with SANs for `localhost`, `127.0.0.1`, and `::1`. The cert and key are written to `{state_dir}/tls/` (where `state_dir` is the parent of `data.path`). Clients connecting to a self-signed server need `insecure = true` in their config or the `--insecure` flag.

The `tls_reload_interval_secs` setting polls the cert/key files for content changes and hot-reloads them without restarting the server.

### `[data]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `path` | string | *(required)* | Parquet data directory |

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
trawld migrates this database automatically at boot (it is the sole writer) and holds a session advisory lock for its lifetime — a second trawld against the same database fails startup instead of racing. Provision a separate database whose role owns the schema; see the [fleet-auth cutover runbook](/reference/fleet-auth-cutover/) for the exact role/grants.
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
| `default_env` | string | `"prod"` | Fills a missing `env` on ingested events (repair code `env.defaulted`). Must pass the env charset and be a member of `envs` |
| `envs` | string list | `[default_env]` | Environment allowlist (ADR-0009). Events with an unlisted `env` hard-reject. Entries must match `[a-z0-9_-]{1,32}`; `wal` and `scheduled` are reserved. Validated at load — trawld refuses to start otherwise. The allowlist gates writes, not reads: removing an env stops new ingest but its directories stay queryable and age out normally |
| `trusted_relays` | CIDR list | `[]` | Peers (collectors/relays) whose address must never be stamped as an event's `host`: a host-less event from one of these is rejected instead of peer-repaired. Invalid entries are boot-fatal |

### `[retention]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `max_age_days` | integer | `90` | Delete data older than N days; `0` disables |
| `min_free_disk_bytes` | byte size | `"1G"` | Delete oldest data when free disk drops below; `0` disables |
| `retention_interval_secs` | integer | `3600` | Retention check frequency (default: 1 hour) |

Disk-pressure deletion is suppressed while a pre-cutover `data.pre-schema-v2/` set-aside directory exists (it sits outside `data/`, so deleting partitions could never reclaim it); each tick under pressure logs `retention_disk_pressure_suppressed` instead. Remove the set-aside to reclaim the space and re-enable the policy. Age-based retention is unaffected.

### `[scheduler]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `true` | Enable scheduled query execution |
| `poll_interval_secs` | integer | `10` | Check for due schedules |
| `report_max_rows` | integer | `10000` | Max rows stored per report run |
| `max_runs_per_schedule` | integer | `100` | Completed runs kept per schedule |
| `report_retention_days` | integer | `30` | Delete report runs older than N days |

### `[web]`

Browser-facing session proxy (`trawl-web` binary). Reads the same `trawld.toml` and runs as a separate systemd unit (`trawl-web.service` on Debian, sidecar container in the Helm chart).

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `bind_addr` | string | `"127.0.0.1:8090"` | Listen address. Defaults to loopback — front with a reverse proxy for external access |
| `upstream_url` | string | derived from `[server].http_addr` | trawld URL. Wildcard binds are rewritten to loopback |
| `cookie_secret_path` | string | (none) | Path to a file holding the 32-byte AEAD cookie-encryption key |
| `cookie_secret_env` | string | (none) | Env var holding the base64-encoded key. Takes precedence over `cookie_secret_path` |
| `session_ttl_secs` | integer | `86400` | Browser session lifetime (24h default) |
| `allow_insecure_cookies` | bool | `false` | Drop `Secure` flag on session cookies. Set true **only** when the proxy sits behind a TLS-terminating reverse proxy |
| `shared_domain` | string | (none) | Parent domain for the shared `fleet_session` SSO cookie, e.g. `".fleet.lab.ktle.net"`. Mirrors coastwatch's `session.shared_domain` — set the same value in both apps. Unset/empty → origin-scoped cookie (standalone mode) |

If neither `cookie_secret_path` nor `cookie_secret_env` is set, the proxy generates an ephemeral key on each startup — sessions won't survive restart. The Debian `trawld` package generates a persistent key at `/var/lib/trawl/web.cookie` automatically via its `postinst` script.

Setting `shared_domain` enables fleet-wide single sign-on: the session cookie is scoped to the parent domain and every fleet app under it accepts it, provided all apps share the same session key (see the [fleet-auth cutover runbook](/reference/fleet-auth-cutover/) for key provisioning). Login and logout validate the `Origin` request header against the request `Host` only; a present Origin whose host differs is rejected with 403. Sharing a parent-domain cookie is deliberately **not** an origin allowlist — a sibling fleet app is a different origin and cannot POST to trawl's auth endpoints, so a compromised sibling can't forge a logout that clears `fleet_session` fleet-wide. **The origin check trusts the request `Host` header** — a reverse proxy in front of `trawl-web` must forward the original `Host`, or legitimate same-origin logins will be rejected.

API clients using bearer tokens (the CLI, `trawl-client`, vector) talk to trawld directly on port 5514 — the proxy only handles cookie-authed browser traffic and blocks `/api/v1/ingest` outright.

```toml
[web]
bind_addr = "127.0.0.1:8090"
cookie_secret_path = "/var/lib/trawl/web.cookie"
session_ttl_secs = 86400
```

### `[syslog]`

Native syslog listener for receiving logs from network appliances.

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
| `allow_cidrs` | string array | `[]` | Source IP allowlist in CIDR notation |
| `source_service_map` | map | `{}` | Source IP → service name mapping |

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
