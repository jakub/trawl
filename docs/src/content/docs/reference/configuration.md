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

Per-role rate limiting in requests per minute. Set to `0` to disable for a role.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `admin` | integer | `100` | Admin role rate limit |
| `analyst` | integer | `60` | Analyst role rate limit |
| `reader` | integer | `30` | Reader role rate limit |
| `ingest` | integer | `1000` | Ingest role rate limit |

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
| `database_url` | string | *(required unless `DATABASE_URL` is set)* | Fleet-auth postgres keystore URL — API keys and roles live here. The `DATABASE_URL` environment variable takes precedence |
| `db_path` | path | *(required)* | Transitional SQLite store for query history, saved queries, and schedules. Must be a fresh file (e.g. `store.db`) — trawld refuses to start on the legacy `auth.db` |
| `audit_interval_secs` | integer | `30` | Poll the fleet keystore for key changes from `fleet-admin`; `0` disables |

:::note
trawld will not start until the fleet database is reachable and migrated (`fleet-admin migrate`). Key revocation takes effect immediately — liveness is checked in postgres on every request (the old `auth_cache_ttl_secs` token cache is gone). See the [fleet-auth cutover runbook](/reference/fleet-auth-cutover/) for migrating an existing deployment.
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

### `[retention]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `max_age_days` | integer | `90` | Delete data older than N days; `0` disables |
| `min_free_disk_bytes` | byte size | `"1G"` | Delete oldest data when free disk drops below; `0` disables |
| `retention_interval_secs` | integer | `3600` | Retention check frequency (default: 1 hour) |

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

If neither `cookie_secret_path` nor `cookie_secret_env` is set, the proxy generates an ephemeral key on each startup — sessions won't survive restart. The Debian `trawld` package generates a persistent key at `/var/lib/trawl/web.cookie` automatically via its `postinst` script.

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
admin = 100
analyst = 60
reader = 30
ingest = 1000

[data]
path = "/var/lib/trawl/data"

[auth]
database_url = "postgres://fleet:CHANGE_ME@db.internal:5432/fleet"
db_path = "/var/lib/trawl/store.db"

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
