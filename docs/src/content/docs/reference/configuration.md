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

### Check the installation with `trawld --doctor`

```bash
trawld --doctor --config /etc/trawl/trawld.toml
trawld --doctor --config /etc/trawl/trawld.toml --format json
```

`--doctor` checks, from the server host, whether trawld will start and serve
with this configuration. It reads the configuration file and the process
environment, connects to the two databases and to trawld's own listener, and
prints one row per check. `--format` takes `table` or `json`. Without
`--format`, the doctor prints a table when standard output is a terminal and
JSON otherwise. Both formats carry the same facts, and the JSON form is
versioned.

The command requires `--config PATH` on the command line. `TRAWL_CONFIG` alone
is a usage error and exits with code 2. `--doctor` does not read another
process's environment and does not source a systemd `EnvironmentFile`. Run it
as the service user and with the service's environment, as
[Verify the installation](/operate/deployment/#verify-the-installation) shows
for each install channel. To check a server from a client host, use
[`trawl doctor`](/reference/cli/#doctor-mode).

#### Checks

A check runs only when its prerequisite completed. Otherwise the check is
`not_sampled` with the reason `blocked`, and its row names the prerequisite in
`blocked_by`. The checks run in the order below.

| Check | Asserts | Prerequisite |
|-------|---------|--------------|
| `server.config` | The file loads and validates as `--check-config` checks it. | none |
| `server.identity` | Names the effective user and uid of the run. | none |
| `server.fleet.connect` | A connection to the Fleet database authenticates. | `server.config` |
| `server.fleet.schema` | The Fleet migration ledger is current. | `server.fleet.connect` |
| `server.app.connect` | A connection to the app-state database authenticates. | `server.config` |
| `server.app.schema` | trawld's boot admits the app-state migration ledger. | `server.app.connect` |
| `server.app.writer` | Reports whether a session holds trawld's writer lock. A held lock does not prove that trawld runs on this host. | `server.app.connect` |
| `server.data.root` | The data root exists and is a directory, or boot creates it, and the running user can use it. On an ingest node, the same holds for the WAL directory, even when `[ingest] wal_dir` names a path inside the data root: the running user can read and write it, or create it in the nearest directory above it that exists. The access check covers the directories trawld writes and the directory where it creates them. It does not cover every parent directory that boot opens to sync. | `server.config` |
| `server.data.epoch` | The data root's `EPOCH` is current, or boot initializes it. | `server.data.root` |
| `server.data.identity` | The data root belongs to the catalog in the app-state database. | `server.data.epoch`, `server.app.schema` |
| `server.data.conformance` | Conformance is recorded for this catalog and data root, or boot runs the pass. A query-only node reports `not_configured`. | `server.data.identity` |
| `server.recovery.repin` | There is no repin marker, or one whose phase boot completes. | `server.data.epoch` |
| `server.recovery.publication` | The publication and rollup markers are readable and well-formed. | `server.data.epoch` |
| `server.tls.material` | The certificate and key parse, match, and are in date, or boot generates them. A directory at `tls/key.pem`, where an older trawld kept its key, fails the check, because boot cannot remove it. When boot would generate the pair, the check also fails if the running user cannot create `tls/` or `tls-key/` in the directory above it, or if either directory is on a read-only filesystem. | `server.config` |
| `server.listener.identity` | The listener presents exactly the certificate on disk. | `server.tls.material` |
| `server.listener.health` | The health endpoint answers, and trawld does not report itself `unavailable`. An `unavailable` answer fails this row, whatever the reported checks say. One row per reported check follows as `server.listener.health.<key>`. Only the checks trawld reports get a row. Any other check names share one `failed` `server.listener.health._invalid` row, which does not show them. | `server.listener.identity` |

The listener checks connect to trawld's own listener without sending an API
key. They accept only the certificate that the configuration names or that
trawld generated, and they make no claim about host names. `trawl doctor`
proves host names from a client.

#### Outcomes

| Outcome | Meaning |
|---------|---------|
| `complete` | The doctor observed the assertion hold. |
| `failed` | The doctor observed evidence against the assertion. Fix it before you start trawld. |
| `not_configured` | The configuration turns off what the check needs, such as conformance on a query-only node. |
| `not_sampled` | The doctor could not look. The `reason` field says why. |

A `not_sampled` row is neither a pass nor a failure. Its reason is one of the
stable codes, among them `blocked`, `permission_denied`, `timed_out`,
`not_listening`, `migration_in_progress`, `unproven`, and `ran_as_root`. Only
`trawld --doctor` reports these codes:

| Reason | Meaning |
|--------|---------|
| `session_not_read_only` | The database session did not start read-only with the doctor's timeouts, so the doctor sent no other query. Check the URL's `options` and `PGOPTIONS`. Connect to the database server directly, not through a pooler that drops startup options. |
| `connection_lost` | A database connection broke after it authenticated, or before the server answered the doctor's startup message. |
| `query_failed` | A read-only database query returned an error after the connection authenticated. |
| `protocol_error` | The database server sent a message the database driver could not decode. The doctor dropped that connection and still ran the other checks. Check that the URL names a PostgreSQL server. |
| `not_postgres` | With `failed`: under `sslmode=disable` or `allow`, the server that a database URL names did not answer the doctor's startup message as a PostgreSQL server does, so the doctor did not connect. Check the database URL's host and port. |
| `interrupted` | The listener closed or reset the connection before the TLS handshake or the health answer was whole. |
| `ambiguous_address` | The listener address resolves to more than one address, and none of them served the certificate on disk. Set `[server] http_addr` to the one IP address and port that trawld listens on. |

Some state exists only because trawld's first start creates it: an empty or
behind app-state schema, an absent or empty data root on an ingest node, and
an absent generated certificate. For that state the doctor asks whether the
boot will accept it. When it will, the check is `complete` with the reason
`will_initialize`. Every state the boot refuses is `failed`: an empty or behind
Fleet schema (run `fleet-admin migrate`), an unsupported epoch, an owned root
without an epoch, and a dirty, ahead, or foreign migration ledger.

A fresh installation that has not started yet reports `will_initialize` rows,
and its listener is `not_sampled` with the reason `not_listening`. It exits
with code 3. It exits with code 1 only when something must change before the
first start.

A migrator that holds its lock while a schema is fresh or behind gives
`not_sampled` with the reason `migration_in_progress`. A current schema is
`complete` even while a migrator holds its lock. A dirty ledger is always
`failed`.

A catalog mismatch is `failed` on every node type. The data root belongs to
another catalog than the app-state database. Choose one way out:

- Point the app-state database at the catalog that owns the archive.
- Restore the app-state dump and the data archive from the same backup.
- Point `[data] path` at a new root.
- Start trawld to adopt the archive on purpose. This is lossy when the catalog
  already pins fields.

On an ingest node the boot adopts the archive through its conformance pass
instead of refusing it, so the doctor is stricter than the boot here. Adopting
is a decision for you to make, not for the doctor.

#### Exit codes

| Code | Meaning |
|------|---------|
| `0` | `pass`: every check is `complete` or `not_configured`. |
| `1` | `fail`: at least one check is `failed`. |
| `2` | Usage error, such as a missing `--config`. |
| `3` | `incomplete`: no check failed, but at least one was `not_sampled`. |

A failure outweighs a check that could not look. The same codes apply to
`trawl doctor`.

#### Run as root

Root reads what the service user may not, so a root run cannot prove that the
service user has access. An access check, `server.data.root`, is `not_sampled`
with the reason `ran_as_root`. It still fails when the data root, or a WAL
directory outside it, is not a directory, or is absent and cannot be created
there. It does not ask what the running user may do with either directory, and
so does not report a read-only filesystem. In a root run, `server.tls.material`
does not ask whether the running user can create or write `tls/` and
`tls-key/`. It reports `ran_as_root` when a generated directory or the key
belongs to another uid, because a root run cannot know which user trawld runs
as. Content checks still report what they read: a certificate parses, an epoch
is current. A check that waits on the access check still runs. A root run never exits `0`. Run the doctor as the service user to
get an answer about access.

#### Side effects

The doctor writes nothing and takes no lock. It runs no migration and starts
no crash-dump capture, even when `TRAWL_CRASH_DUMP_DIR` is set. It opens
database sessions as read-only and reads only bounded amounts of data in
bounded time. It never sends an API key and never sends an event. It reads
each database TLS file once and passes its contents to the database driver.

The doctor trusts what the configuration selects, as trawld does: the database
servers that the URLs name, and the credential files that you selected. The
database driver does not bound what a server sends, so a hostile database
server can make the doctor reserve a large amount of memory, as it can with
trawld. The password-file check guards against mistakes, not against a file
that someone replaces while the doctor runs.

A URL can name the wrong kind of server by mistake, such as an HTTP or SSH
service. With `sslmode=disable` or `allow`, the database driver would read that
server's first answer as the length of a very large message. So under those
two modes the doctor first opens its own connection to the same address and
sends the startup message that the driver sends, with no password. A server
of the wrong kind receives the user and database names, as it would from the
driver. The doctor reads the first 5 bytes of the answer. If the message type
is not `R`, `E`, or `v`, or the length is more than 8 KiB, the connect check
fails with the reason `not_postgres`. Otherwise the doctor reads the rest of
the answer, closes its side of the connection, and waits for the server to
close. Under every other `sslmode`, the driver sends an `SSLRequest` itself and
refuses such a server.

On a real PostgreSQL server, this check is one more login attempt, with the
same user, database, and read-only settings as the doctor's session:

- With a method that sends a challenge first, such as SCRAM, the server logs
  only the connection, and only when `log_connections` is on.
- With `trust` or `peer`, the server opens a brief read-only session and logs
  it as it logs any session.
- With PAM, a lockout module such as `pam_faillock` can count one failed
  login each time the doctor checks the database this way.

One other side effect remains. A probe that makes the running trawld answer
503 emits that server's `http_failure` telemetry event
([ADR-0040](https://github.com/jakub/trawl/blob/main/docs/adr/0040-every-server-failure-names-its-request-and-stage.md)).

#### What the output never contains

Rows name sources such as `FLEET_DATABASE_URL from the environment`, never
values. The output has no URLs, host names, passwords, certificate details,
catalog identifiers, listener addresses, or driver and operating-system error
text. It may name the configuration and credential files the run selected, and
the running user and uid.

### Check the web proxy with `trawl-web --doctor`

```bash
trawl-web --doctor --config /etc/trawl/trawld.toml
trawl-web --doctor --config /etc/trawl/trawld.toml --format json
```

`--doctor` checks, from where the web proxy runs, whether `trawl-web` can
start with this configuration, keep browser sessions across a restart, and
reach trawld with a verified certificate. It reads the configuration file and
the process environment, sends one health request to trawld through the
proxy's own client, and prints one row per check. `--format` takes `table` or
`json`. Without `--format`, the doctor prints a table when standard output is
a terminal and JSON otherwise. The JSON form is the versioned report that
`trawld --doctor` prints, with `vantage` set to `web`.

The command requires `--config PATH` on the command line. `TRAWL_CONFIG`
alone is a usage error and exits with code 2. `--doctor` does not read another
process's environment and does not source a systemd `EnvironmentFile`. Run it
as the service user and with the service's environment, as
[Verify the installation](/operate/deployment/#verify-the-installation) shows
for each install channel. `trawl-web --doctor` does not check trawld's
databases, data, or listener certificate. `trawld --doctor` checks those.

#### Checks

A check runs only when its prerequisites completed. Otherwise the check is
`not_sampled` with the reason `blocked`, and its row names the prerequisite in
`blocked_by`. The checks run in the order below.

| Check | Asserts | Prerequisite |
|-------|---------|--------------|
| `proxy.config` | The file reads and parses as the whole `trawld.toml` schema, and the listen address is a `host:port` that resolves, as at startup. A file that does not parse, or a listen address that does not resolve, fails this check and exits with code 1, not 2. | none |
| `proxy.identity` | Names the effective user and uid of the run. | none |
| `proxy.public_origins` | A non-empty list of valid browser origins resolves. The row says whether the list came from `[web] public_origins` or from `FLEET_SESSION_PUBLIC_ORIGINS`, and whether the variable replaced the file's list. An empty list or an invalid entry fails. | `proxy.config` |
| `proxy.cookie_settings` | The session cookie's domain, path, `Secure` flag, and lifetime resolve. The row names the source of each. With `Secure` off, the check fails when any public origin is not loopback. Loopback hosts are `localhost`, `127.0.0.0/8`, and `[::1]`. | `proxy.config`, `proxy.public_origins` |
| `proxy.cookie_key` | A persistent cookie key source is selected and usable. The row names the source: `FLEET_SESSION_AEAD_KEY`, the variable that `cookie_secret_env` names, or the `cookie_secret_path` file. A `cookie_secret_path` file of exactly 32 bytes that the running user can read is `complete`. A file of another length, or a variable that is unset or does not hold a base64 key, fails. A file that the running user cannot read is `not_sampled` with the reason `permission_denied`. With no source, the check is `not_configured` with the reason `ephemeral_each_start`. | `proxy.config` |
| `proxy.upstream.trust` | The upstream URL is `https` and carries no user name or password, `upstream_connect_addr` suits it, and the trust resolves. The trust is the platform trust store, or the `upstream_ca_path` file when it parses as PEM certificates. A CA file that does not exist yet is `not_sampled` with the reason `ca_not_present`. A CA file that the running user cannot read is `not_sampled` with the reason `permission_denied`. | `proxy.config` |
| `proxy.upstream.health` | `GET /api/v1/health` to trawld, through the proxy's own client and `upstream_connect_addr`, verifies trawld's certificate and returns trawld's health answer. An `unavailable` answer fails this row, whatever the reported checks say. One row per reported check follows as `proxy.upstream.health.<key>`. Only the checks trawld reports get a row. Any other check names share one `failed` `proxy.upstream.health._invalid` row, which does not show them. | `proxy.upstream.trust` |

The health request takes the same path from `trawl-web` to trawld as a
signed-in browser request. It trusts only what `proxy.upstream.trust` resolved,
and it checks the host name in `upstream_url`. The doctor reports each way the
request can fail:

- The connection is refused: `failed` with the reason `connection_refused`.
- trawld's certificate does not verify: `failed` with the reason
  `certificate_not_trusted`.
- The answer is a redirect: `failed` with the reason `redirect_refused`.
- The TLS handshake fails for another reason: `failed`, with a sentence that
  says so.
- The answer has another HTTP status, such as 502 from a proxy in between:
  `failed`, naming the status. The doctor never reports such an answer as a
  certificate problem.
- The connection or the answer takes too long: `not_sampled` with the reason
  `timed_out`.
- trawld's corpus is still recovering after a restart: `not_sampled` with the
  reason `recovering`.

#### Outcomes

| Outcome | Meaning |
|---------|---------|
| `complete` | The doctor observed the assertion hold. |
| `failed` | The doctor observed evidence against the assertion. |
| `not_configured` | The configuration selects no persistent cookie key, so sessions end at each restart of `trawl-web`. |
| `not_sampled` | The doctor could not look. The `reason` field says why. |

A `not_sampled` row is neither a pass nor a failure. Its reason is one of the
stable codes, among them `blocked`, `permission_denied`, `timed_out`,
`too_large`, `interrupted`, `unreadable`, `recovering`, `ca_not_present`, and
`ran_as_root`.
Only `trawl-web --doctor` reports these codes:

| Reason | Meaning |
|--------|---------|
| `ca_not_present` | With `not_sampled`: the `upstream_ca_path` file does not exist yet. trawld writes its certificate at its first start. Start trawld, then run the doctor again. Until the file appears, `trawl-web` answers requests that need trawld with 503. |
| `ephemeral_each_start` | With `not_configured`: no cookie key source is selected, so `trawl-web` generates a new key at each start and every earlier session ends. Set `cookie_secret_path` so sessions survive restarts. |
| `connection_refused` | With `failed`: the upstream address refused the connection, as when trawld does not run or listens elsewhere. Check that trawld runs, and check `upstream_url` and `upstream_connect_addr`. |
| `certificate_not_trusted` | With `failed`: trawld's certificate did not verify under the trust that `proxy.upstream.trust` resolved. The CA does not sign it, or it does not name the host in `upstream_url`. The doctor does not retry. |
| `redirect_refused` | With `failed`: the upstream answered with a redirect. The doctor does not follow it and sends nothing to its target. trawld's health endpoint does not redirect, so `upstream_url` or `upstream_connect_addr` reaches another server. |

A run with no cookie key source can exit with code 0, because
`not_configured` is not a failure. When `upstream_ca_path` names the
certificate that trawld generates, as the Debian package configures, the file
does not exist before trawld's first start. `proxy.upstream.trust` is then
`not_sampled` with the reason `ca_not_present`, `proxy.upstream.health` is
`blocked`, and the run exits with code 3.

#### Exit codes

| Code | Meaning |
|------|---------|
| `0` | `pass`: every check is `complete` or `not_configured`. |
| `1` | `fail`: at least one check is `failed`. |
| `2` | Usage error, such as `--config` missing from the command line. |
| `3` | `incomplete`: no check failed, but at least one was `not_sampled`. |

A failure outweighs a check that could not look. The same codes apply to
`trawld --doctor` and `trawl doctor`.

#### Run as root

Root reads what the service user may not, so a root run cannot prove that
`trawl-web`'s user can read its files. In a root run, `proxy.identity` is
`not_sampled` with the reason `ran_as_root`. This differs from
`trawld --doctor`, whose identity row stays `complete`. So a root run of
`trawl-web --doctor` never exits `0`, even when the key comes from the
environment and the trust is the platform trust store.

With a `cookie_secret_path` file, a root run reports `proxy.cookie_key` as
`not_sampled` with the reason `ran_as_root` where it would be `complete`. The
check still fails when the file's content is wrong, such as a key that is not
32 bytes. A key from the environment is content, so its row does not change.
Run the doctor as the service user to get an answer about access.

#### Side effects

The doctor never binds the listen address, never generates a session key, and
writes nothing. It logs no configuration value, touches no database, and
reads each file once: the configuration, the cookie key file, and the CA file.
Each read has a size limit and a time limit. A host name in the listen address
resolves through the system resolver, as at startup, within a time limit.
Because the doctor never binds the listen address, an address that another
process holds passes here and still stops `trawl-web` at startup.

It sends one request to trawld, `GET /api/v1/health`, with no API key, no
`Authorization` header, and no cookie. It follows no redirect, does not retry,
and ignores proxy variables such as `HTTPS_PROXY`. It reads at most 64 KiB of
the answer, and the connection and the answer each have a time limit. When
`upstream_url` carries a user name or password, `proxy.upstream.trust` fails
and the doctor sends no request.

One other side effect remains. A health request that makes the running trawld
answer 503 emits that server's `http_failure` telemetry event
([ADR-0040](https://github.com/jakub/trawl/blob/main/docs/adr/0040-every-server-failure-names-its-request-and-stage.md)).

#### What the output never contains

Rows name sources such as `FLEET_SESSION_PUBLIC_ORIGINS` or the config file,
never values. The output has no origins, host names, upstream URL, listen
address, certificate subjects or alternative names, key bytes, or TOML and
operating-system error text. It may name the configuration, cookie key, and CA
files the run selected, and the running user and uid. It names the variable
that `cookie_secret_env` selects only when that name is a plain variable name
that does not decode as a key.

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

- `tls_cert_path` and `tls_key_path` must both be set or both omitted. With both omitted, trawld generates a self-signed ECDSA P-256 certificate at startup, with SANs for `localhost`, `127.0.0.1`, and `::1`. The certificate is `{state_dir}/tls/cert.pem`, mode 0644, in a 0755 directory, and the key is `{state_dir}/tls-key/key.pem`, in a 0700 directory. `state_dir` is the parent of `[data] path`.
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
daemon paths accordingly. See [configure the daemon API certificate](/operate/deployment/#configure-the-daemon-api-certificate)
for complete setup and verification instructions. Browser-ingress TLS remains
a separate setting under `ingress.tls`.

With `web.enabled`, the chart also sets how the `trawl-web` sidecar verifies
trawld:

| `tls.mode` | Values the sidecar needs | Rendered `[web]` keys |
|------------|--------------------------|-----------------------|
| `auto` | None. `tls.upstreamServerName` and `tls.upstreamCa` are refused | `upstream_ca_path = "{state_dir}/tls/cert.pem"`, from the data volume's `tls/` directory mounted read-only |
| `secret` | `tls.upstreamServerName` and `tls.upstreamCa` | `upstream_url = "https://<upstreamServerName>:<port>"`, `upstream_connect_addr = "127.0.0.1:<port>"`, and `upstream_ca_path` unless `upstreamCa` is `system` |
| `certManager` | `tls.upstreamCa`. `tls.upstreamServerName` defaults to the first `dnsNames` entry that is not a wildcard | The same keys as `secret` |

`tls.upstreamCa` accepts `secret`, which pins `ca.crt` from the TLS Secret,
`system`, which uses the platform roots, or the absolute path of a CA file
that you mount with `web.extraVolumes` and `web.extraVolumeMounts`.

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
| `compaction_interval_secs` | integer | `10` | How often the WAL-to-parquet compaction task runs. Must be greater than `0` when ingest is enabled |
| `internal_telemetry` | bool | `true` | Write server events into the ingest pipeline as `service=trawld` |
| `daily_rollup` | bool | `true` | Merge hourly parquet files into daily files for older dates |
| `event_bus_capacity` | integer | `4096` | Broadcast channel capacity behind SSE. A full channel makes slow subscribers lag |
| `hot_buffer_max_events` | integer | `100000` | Events the hot buffer holds. A write that does not fit is refused, and admitted events stay until compaction drains them. HTTP and syslog may fill 15/16 of the cap, and internal telemetry may fill all of it. See [hot-buffer admission](/architecture/data-flow/#hot-buffer-admission) |
| `hot_buffer_max_bytes` | byte size | `"100M"` | Serialized ndjson bytes the hot buffer holds, admitted the same way as `hot_buffer_max_events`. A write must fit both caps |
| `stats_interval_secs` | integer | `60` | How often server stats are emitted as telemetry. `0` disables |
| `telemetry_flush_interval_secs` | integer | `1` | How often buffered tracing events are flushed to the WAL |
| `telemetry_buffer_max_bytes` | byte size | `"16M"` | One estimated memory budget for everything self-telemetry holds while the WAL is unhealthy: active buffer, retry queue, and the batch in flight. Minimum `"64K"` |
| `compaction_chunk_size` | integer | `500` | Maximum WAL files merged per compaction chunk. A larger backlog is split into chunks. At most `4096`, because each chunk's publication marker names every file it merges |
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
- Boot [hydration](/architecture/data-flow/#hydration) loads the surviving WAL into the hot buffer against `hot_buffer_max_events` and `hot_buffer_max_bytes`. If you lower either cap across a restart, a backlog that fitted before can stop fitting. What does not fit is overhang: searches answer 503 `corpus_recovering` until compaction drains it. This happens once, after that restart.
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
| `upstream_url` | string | derived from `[server] http_addr` | How the proxy reaches trawld. A wildcard bind is rewritten to loopback. Must be `https` and must hold no user name, password, query, or fragment. A path prefix is allowed. Any of these faults is a startup error, and the error does not quote the URL |
| `upstream_ca_path` | path | *(none)* | PEM file of the CA certificates to trust for trawld. The proxy then trusts only these CAs and still checks the hostname. Unset means the platform trust store. A file that does not exist yet is accepted at startup: until it appears, requests that need trawld get 503 `upstream certificate not available`. The file is read again every 30 seconds and on the first request after it appears, so a replaced CA loads without a restart. An empty path or a file that does not parse is a startup error. A later change that does not parse keeps the last good CA |
| `upstream_connect_addr` | string | *(none)* | IP address and port to connect to, such as `"127.0.0.1:5514"` or `"[::1]:5514"`, while TLS verifies the host in `upstream_url`. Requires an `upstream_url` whose host is a DNS name, and the same port as `upstream_url` (443 when the URL has none) |
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
- The proxy always verifies trawld's certificate, against the platform trust store or the `upstream_ca_path` file. It never follows a redirect from trawld and ignores proxy variables such as `HTTPS_PROXY`. See [Configure TLS](/operate/access/#configure-tls).

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
| `TRAWL_WEB_BIND_ADDR` | Overrides `[web] bind_addr`. A value that is not UTF-8 is a startup error |
| `TRAWL_WEB_UPSTREAM_CA_PATH` | Overrides `[web] upstream_ca_path`. An empty value counts as unset, so the `[web]` key applies. A value that is not UTF-8 is a startup error, not unset |

The Helm chart passes `FLEET_SESSION_PUBLIC_ORIGINS` to the sidecar as well as
rendering `public_origins` into the generated TOML.

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
