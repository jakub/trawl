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
| `query_log` | path | *(none)* | ndjson debug log (one entry per query execution) — see [the query debug log](#the-query-debug-log) |
| `query_log_max_bytes` | byte size | `"100M"` | Query debug log size cap with single-file rollover to `<path>.1`; `0` disables rollover |
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

#### Logging filter (`RUST_LOG`)

trawld's stdout log and its internal telemetry ([`internal_telemetry`](#ingest)) build their filters from one directive string, resolved explicitly at startup:

- **`RUST_LOG` unset** → the shipped default filter:

  ```text
  trawl_server=info,trawld=info,fleet_auth=info,auth.backend=info,storage.backend=info,preauth.transport=info
  ```

  This exact string is a cross-packaging contract — the code fallback, the Helm chart's `logLevel`, and the Debian environment example all carry it. It enumerates every target trawl emits under: `trawl_server` (the library — handlers, ingest, compaction), `trawld` (the binary — startup banner, config warnings, task panics), `fleet_auth` (auth middleware), the deliberately-overridden `auth.backend` / `storage.backend` targets that make backend failures independently alarmable, and `preauth.transport` (the accept loop's TLS-handshake and connection diagnostics, overridden off `trawl_server` so they can be kept out of the corpus — see below). When customizing, keep all six — dropping the backend or transport targets makes those errors invisible. A global `info` is deliberately not the default: it would enable noisy dependency targets.
- **`RUST_LOG` set and valid** → your value is authoritative, verbatim.
- **`RUST_LOG` set but unparseable** → the default filter is installed and exactly one `config_warning` event reports the parse error (never the raw environment value).

A configuration-file failure happens *before* any tracing subscriber exists: it is reported on stderr with the resolved config path, not through telemetry.

**Unmetered rejections are logged, never persisted.** Whatever the directives say, the `fleet_auth`, `auth.backend`, `preauth.transport` and `trawl_server::policy::unmetered` targets are excluded from `service=trawld` telemetry (they still print on stdout, and to `log_file` when telemetry is disabled). The first two come from the bearer middleware, which necessarily runs *before* per-key rate limiting; `preauth.transport` is cheaper still — a bare TCP connect-and-close provokes a `tls_handshake_failed` warning before any request exists; `trawl_server::policy::unmetered` is the 403 for an authenticated key that resolves no trawl permission, which the policy layer also decides outside the limiter (so a grantless key never spends a bucket to be told no). Persisting any of them would let a client no rate limit can slow turn a connection or request flood into durable corpus growth, one record per rejection. Failed authentication, failed handshakes and grantless 403s are therefore stdout/log-pipeline signals; the corpus still carries everything trawld emits behind the limiter, the `storage.backend` alarm target, and the catalog/health events a backend outage produces. Alerting does not depend on that stdout pipeline: rejected requests are counted on `/metrics` as `trawl_auth_failures_total{reason="unauthorized"|"backend_unavailable"|"no_trawl_grant"|"forbidden"|"internal"}`, a closed label set with no key, name or path in it — safe to expose on a flooded endpoint, and the signal to alarm on for credential stuffing or a revoked key still in use.

**`trawl-web` filters separately.** The session proxy is a different process with a different target, so it must never inherit trawld's filter — a target-only filter that omits `trawl_web` silences the proxy completely. Its own default is:

```text
trawl_web=info,fleet_auth=info
```

`trawl_web` carries the proxy's session, origin, upstream and startup diagnostics; `fleet_auth` the shared session/origin primitives. Same contract shape as trawld's: the binary fallback, the Helm chart's `web.logLevel`, and the Debian `/etc/default/trawl-web` example all carry this exact string, and a set-and-valid `RUST_LOG` is authoritative.

#### The query debug log

`server.query_log` (or `TRAWL_QUERY_LOG` / `--query-log`) enables an ndjson debug log with one entry per query execution — built for `tail -f | jq` debugging.

**Sensitivity.** Each entry combines the authenticated identity, the raw DSL, the generated SQL *with parameter values*, source file paths, hot-buffer state, and a sample of result rows — more sensitive than the event corpus it debugs. trawld therefore:

- creates the file **owner-only** (`0600` on Unix) and tightens a pre-existing looser file at open;
- opens it with `O_NOFOLLOW`: a **symlink at the configured path is refused**, not followed — otherwise a local user who can create that path could redirect the log into a file of their choosing and have trawld `chmod` it;
- emits a startup `warn` naming the path and its contents whenever the log is enabled;
- bounds it with `server.query_log_max_bytes` (default 100 MiB): past the cap the file rolls over to a single retained `<path>.1` (also `0600`); `0` disables rollover.

Retention is exactly those two files — there is no multi-generation rotation or age-based cleanup; delete them when done debugging, preferably with trawld stopped. Deleting the *active* file under a running trawld leaves it writing to the unlinked inode (the space is not reclaimed until restart) and makes the rollover rename fail; trawld keeps the entries and re-attempts the rollover only once per `query_log_max_bytes` written, so a broken rotation path costs one `warn` per cap rather than one per query. A rollover whose rename lands but whose reopen fails is undone, so the cap always applies to the file at the configured path; in the one case where the undo fails too, trawld holds a file that path can no longer name, and closes the log (an `error` says so) until restart rather than growing it unbounded. **Point it at a directory only trawld can write** (`/var/lib/trawl/query-debug.log`, say — not `/tmp`): the mode protects the file's contents, but nothing trawld does can protect a path a local user is free to create entries in. Result samples never enter default `service=trawld` telemetry, which since issue #56 carries query metadata (`query_id`, `query_len`, actor, outcome, timing, and a stable `error_class`) but neither raw query text nor error text — for queries, exports, and SSE streams alike. The full text lives in authenticated query history, this debug log, and the DEBUG-only `query_text` / `query_error_text` tracing events.

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
| `telemetry_buffer_max_bytes` | byte size | `"16M"` | One memory budget for everything self-telemetry holds while the WAL is unhealthy — active buffer, retry queue and the in-flight batch (estimated charge, like `hot_buffer_max_bytes`). Enforced as events arrive: over budget the oldest queued batches are shed first, then the incoming event itself, counted in `trawl_telemetry_events_dropped_total{reason="buffer_cap"}` |
| `default_env` | string | `"prod"` | Fills a missing `env` on ingested events (repair code `env.defaulted`). Must pass the env charset and be a member of `envs` |
| `envs` | string list | `[default_env]` | Environment allowlist (ADR-0009). Events with an unlisted `env` hard-reject. Entries must match `[a-z0-9_-]{1,32}`; `wal` and `scheduled` are reserved. Validated at load — trawld refuses to start otherwise. The allowlist gates writes, not reads: removing an env stops new ingest but its directories stay queryable and age out normally |
| `trusted_relays` | CIDR list | `[]` | Peers (collectors/relays) whose address must never be stamped as an event's `host`: a host-less event from one of these is rejected instead of peer-repaired (HTTP) or kept with `host` omitted (syslog). Invalid entries are boot-fatal. Peer addresses are canonicalized before matching — a dual-stack bind's IPv4-mapped peer (`::ffff:10.1.2.3`) matches a plain v4 entry (`10.0.0.0/8`), and a mapped-form entry folds to its v4 meaning at load |
| `severity_from` | source list | `["severity", "severity_text", "level"]` | Wire keys `_severity` derives from, in precedence order — first *mappable* wins. Empty is legal and derives nothing |
| `time_from` | source list | `["_time", "timestamp", "@timestamp"]` | Wire keys `_time` derives from, in precedence order — first *present* wins. Must contain `_time` |

#### Derivation sources (`severity_from` / `time_from`)

`_severity` and `_time` are trawl-owned envelope slots, and these two lists are the whole of what they read. Derivation only **reads**: every source stays exactly where it arrived, as an ordinary queryable column under the name its sender chose, and a derivation into the `_` namespace touches nothing sender-visible, so it is never recorded as a repair.

An entry takes either spelling:

```toml
[ingest]
severity_from = ["severity", "severity_text", "level"]
time_from = ["_time", "timestamp", "@timestamp"]

# The typed form declares the dialect an entry's NUMERICS read in.
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

Changing either list is **forward-only**. There is no policy history and nothing re-derives stored events: an event keeps the `_severity` and `_time` it was written with, and the new lists apply from the next event onward. Reinterpreting an old corpus means a repin, not a config reload.

**Profile-fixed sources are not configurable.** Each producer profile prepends its own transport-proven sources to these lists: the syslog listener contributes `{ field = "syslog_severity", dialect = "syslog" }` and `syslog_timestamp`, while the HTTP and trawld doors contribute none. Those fixed entries win over anything configured under the same name and do not count against the 8-entry bound — they are trawl's, not the operator's. Everything else about the two lists is global and identical at every door.

### `[retention]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `max_age_days` | integer | `90` | Delete data older than N days; `0` disables |
| `min_free_disk_bytes` | byte size | `"1G"` | Delete oldest data when free disk drops below; `0` disables |
| `retention_interval_secs` | integer | `3600` | Retention check frequency (default: 1 hour) |

Both sweeps stand down while a repin job's marker or staging roots exist
(`data/REPIN`, `data.repin-next/`, `data.repin-aside/`): the job
double-holds its affected bytes until its final sweep and pre-flights
against `min_free_disk_bytes` before starting, so retention could neither
relieve the pressure nor safely delete files out from under the shadow
build. They resume the tick after the job (or its boot replay) finishes.
A staging root that survives its sweep — a permission or I/O error —
deliberately keeps the marker, since the marker is what licenses trawl to
delete that root: the cleanup is retried at the next boot
(`repin_recovery_incomplete` meanwhile), and retention stays suppressed
until the root is actually gone. Alert on `trawl_retention_suppressed` —
it is 1 for every tick either sweep stands down and 0 once they run
again, so it distinguishes a repin in progress (minutes, hours) from
staging nothing owns, which holds it at 1 indefinitely while the archive
grows. `trawl_catalog_repin_running` cannot: it is 0 in exactly the
stranded case.

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
