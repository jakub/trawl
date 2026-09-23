---
title: Check health and stalled work
description: Confirm that a trawld server is serving, read what is degraded, and free capacity that a stalled query still holds.
---

These commands address one server. `TRAWL_URL` names its HTTPS API,
`TRAWL_PROFILE` names the matching CLI profile, and `TRAWL_CURL_CONFIG` names
an owner-only curl config file that holds the bearer header. Keep the token in
that file, not on the command line, and do not print the file.

## Check the server

For warnings about reported discards, persistence failures, compaction
failures, and quarantined files, use [Respond to operational alerts](/operate/operational-alerts/).
Those rules complement serving checks; they do not detect silent stalls or
establish recovery when an alert resolves.

1. Ask for health. `/api/v1/health` needs no key.

   ```bash
   curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/health"
   ```

   A serving server answers 200 with `"status":"ok"` and four checks that read `ok`:

   ```json
   {"status":"ok","checks":{"duckdb":"ok","auth_db":"ok","storage_db":"ok","data_path":"ok"},"version":"..."}
   ```

2. Confirm that your key is accepted.

   ```bash
   curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/whoami"
   ```

   A 200 confirms the key. A 401 means the token is invalid or revoked. A 403
   means the key holds no Trawl permission.

3. Run a bounded query.

   ```bash
   trawl -p "$TRAWL_PROFILE" query 'last=15m | head 20'
   ```

   You see a table of up to 20 events. An empty table means nothing arrived in
   the last 15 minutes. A 503 or 504 means the query path is the problem. See
   [Diagnose a 503 or 504 from a query](#diagnose-a-503-or-504-from-a-query).

On a packaged host, `systemctl status trawld trawl-web` and `journalctl -u trawld`
show the process state. On Kubernetes, read the pod events and each container's
log separately, including `init-auth`. Keep Secret contents out of incident notes.

## Read the checks

`status` is `ok`, `degraded`, or `unavailable`. Only a `duckdb` failure makes
the server `unavailable` and the response 503. Any other failing check leaves
the response at 200 with `"status":"degraded"`. Read every check, not the status
alone. The response shape is in [the API reference](/reference/api/#health).

| Check | Meaning when it reads `error` | What to do |
| --- | --- | --- |
| `duckdb` | The query engine did not answer its probe. Every query fails. | Read the trawld journal for engine errors. Restart trawld if the engine does not recover. |
| `auth_db` | The auth database did not answer within the probe timeout. Bearer checks fail and `trawl_auth_failures_total{reason="backend_unavailable"}` rises. | Check the auth database and the `[auth]` settings in `trawld.toml`. |
| `storage_db` | The app-state database did not answer. Saved queries, history, and repin jobs fail. | Check the app-state database and the `[storage]` settings. |
| `data_path` | `[data] path` is missing or is not a readable directory. Cold data is unreadable. | Check the mount and the directory permissions for the `trawl` user. |

## Inspect capacity

1. Read the pool counters.

   ```bash
   curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/stats"
   ```

   Compare `pool_capacity`, `pool_available`, `active_queries`, and
   `pool_retained`. `pool_capacity` is `max_concurrent_queries`, which defaults
   to the CPU count. `pool_retained` counts permits held by work whose request
   has already ended. Retained permits are a subset of the held permits, not an
   extra count.

2. List the work that holds those permits.

   ```bash
   curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/queries"
   ```

   The response has three lists: `active`, `recent`, and `retained`. Each
   `retained` entry carries `id`, `kind`, `started`, and `retained_ms`. `kind`
   is one of `query`, `from_saved`, `export`, `scheduled`, `ping`, or `sample`.
   An entry for key-owned work also carries `user` and `query`. An entry for
   `ping` or `scheduled` work carries them only for a reader with `server_manage`.

3. Watch `trawl_query_permits_retained` on `/metrics`. A value that stays above
   zero means capacity is held by work that no request waits for. Alarm on that
   gauge.

The events `query_permit_retained` and `query_permit_reclaimed` bracket each
retained interval in the trawld log. They carry metadata, never DSL. Field
details are in the API reference under [Running queries](/reference/api/#running-queries)
and [Server info](/reference/api/#server-info).

## Inspect ingestion and storage

1. Open **Health** with a key that has `server_manage`, or read the
   shared dashboard snapshot:

   ```bash
   curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/dashboard"
   ```

2. Check the stream label before using the browser readings. If it reports
   **Stale; reconnecting** or **Live updates failed**, treat the displayed values
   as a retained snapshot. Wait for a new stream snapshot to clear that label.
   A live connection does not prove that a storage measurement succeeded.

3. Read **Syslog configuration** before interpreting its counters. If syslog is
   disabled, use the [syslog setup guidance](/operate/ingestion/#receive-syslog)
   to check the intended configuration. If received counts increase, confirm
   persistence with a bounded query as described in
   [Confirm delivery over time](/operate/ingestion/#confirm-delivery-over-time).
   HTTP readings exclude syslog, and receive counts alone do not prove persistence.

4. Read each storage measurement's status and age before using its totals.
   Treat **Awaiting measurement** and **Measurement unavailable** as unknown
   storage usage. If collection failed after a successful scan, use the retained
   totals only with their displayed age. Check the configured root and its mount
   using [the data-path guidance](#read-the-checks). An absent query-only archive
   is not a measured empty directory.

5. Compare compaction readings across new snapshots. Use
   [storage recovery](/architecture/recovery/) and
   [retention guidance](/operate/retention/) when investigating storage behavior.
   Do not infer a current incident from an error tally accumulated since startup.
   Successful cycles can contain no eligible work, and error tallies can accompany
   successful cycles. A missing last-success age means no successful cycle
   reported since startup; an age of zero is a reported success.

The [dashboard reference](/reference/api/#dashboard-snapshot) defines the four
measurement states and the scope of each count. In the terminal's **DATA
PIPELINE** panel, `failed age 120s 7f/91 B` means seven files and 91 bytes from
the last complete sample, aged 120 seconds at the displayed snapshot.
`not configured`, `awaiting measurement`, and `failed; unavailable` do not
represent measured zero. The browser and terminal keep the supplied sample age
unchanged until they receive another dashboard snapshot.

If you use Prometheus, read the dashboard metadata when you need storage sample
status or age. The four [storage gauges](/reference/api/#prometheus-metrics)
retain last complete totals after collection failure and have no status or age
signal. Flat values alone cannot confirm collector health.

## Diagnose a 503 or 504 from a query

`timeout_secs`, 30 seconds by default, starts one deadline right after
authentication, and that deadline covers admission, queueing, and execution.
Where it expires decides the status code.

| Symptom | Meaning | What to do |
| --- | --- | --- |
| 503 with `server at capacity: the query was not started` | The deadline expired before any database work started. Nothing ran, and query history has no row. | Read `pool_retained` and the `retained` list. Cancel retained work, or raise `max_concurrent_queries`. |
| 504 with `query timed out` | The deadline expired after work started. The bind or scan can still hold its permit. | Find the id in `retained` and cancel it. |
| `trawl_query_permits_retained` stays above zero | Finished requests still occupy the pool. | Cancel each retained id. |

To cancel, send `DELETE /api/v1/queries/{id}` with `server_manage`, or with
`query_cancel` when your key submitted the query. The response
`{"cancelled":true,"query_id":N}` acknowledges the request without proving that
the work stopped, and a repeated cancel changes nothing. trawld records a cancel
that arrives during the bind and checks it again before execution. A bind
already inside DuckDB cannot be interrupted, so its permit returns only when the
bind returns.

## Trace a server failure

Symptom: a client received a 5xx, and you need to know which request failed,
where, and why.

Every 5xx that trawld returns, `/api/v1/health` included, emits one event with
`event_type=http_failure`. The event names its request with explicit fields. It
does not inherit them from the request span, so it never carries the raw path or
the user agent. It never carries error text either, because that text can hold
generated SQL, event values, and the caller's DSL.

| Field | Value |
| --- | --- |
| `request_id` | The ULID that the response returned in its `X-Request-Id` header. |
| `method` | A standard HTTP method name, or `OTHER`. |
| `route` | The matched route template, such as `/api/v1/query`. `<unmatched>` when no route matched. Never the raw path. |
| `status` | The status code sent. |
| `latency_ms` | Time from the start of the request to the response. |
| `stage` | How far the request got. See the next table. |
| `reached` | On `stage=unrecorded` only: `pre_admission`, `admitted`, or `handler`, the last point the request passed. |
| `error_class` | The server's closed error class. `panic` for a caught panic, `unknown` when nothing was recorded. |
| `cause_kind` | A closed kind taken from the typed error beneath the class: an I/O error kind such as `io_storage_full`, a DuckDB kind such as `duckdb_failure`, a Postgres kind such as `pg_pool_timed_out`, or `auth_worker`. `none` when no cause was recorded. |
| `query_id` | Present when the request allocated a query ID. |
| `key_id` | Present when a rate limiter metered the request. Names the key it metered. |
| `peer_addr` | Present when no rate limiter metered the request. The client address, the only lead when no key is known. |

| `stage` | Meaning |
| --- | --- |
| `pre_admission` | The request failed before any rate limiter admitted it, for example with the auth backend down. |
| `admitted` | The request failed after the rate limiter admitted it, before the handler. |
| `handler_error` | The handler returned a typed error. |
| `panicked` | trawld caught a panic while it served the request. |
| `unrecorded` | The response is a 5xx, but its producer recorded no failure. `reached` points at that producer. |

A 503 or 504 logs at WARN, because both are expected pressure outcomes. See
[Diagnose a 503 or 504 from a query](#diagnose-a-503-or-504-from-a-query).
Every other 5xx logs at ERROR.

To trace one failure:

1. Read the `X-Request-Id` header of the failed response, or ask the client
   for it.
2. Find the failure event and the events logged inside the same request.

   ```bash
   trawl -p "$TRAWL_PROFILE" query 'service=trawld request_id=01K5EXAMPLE0000000000000000 last=24h | table _time, event_type, _severity, route, stage, reached, error_class, cause_kind, query_id'
   ```

3. If the failure event carries a `query_id`, find the query lifecycle events
   for that ID. `query_failed` names the query ID and its own `error_class`.

   ```bash
   trawl -p "$TRAWL_PROFILE" query 'service=trawld query_id=4182 last=24h | table _time, event_type, _severity, error_class, duration_ms'
   ```

4. For a failure with `stage=unrecorded`, read `reached`. The producer that
   sent the 5xx without recording why sits after that point, and that
   producer is the bug to report.

Not every failure event persists:

- A metered failure event always persists. The per-key rate limiter already
  bounds how many of them one client can cause.
- An unmetered 5xx persists under one process-wide cap of 60 events per
  minute. The cap is fixed in code and has no setting. Past the cap, the event
  goes to stdout only, and `trawl_telemetry_events_dropped_total` counts it under
  `reason="unmetered_cap"`. The `telemetry_dropped` event in stored telemetry
  reports the same count as `dropped_events_unmetered_cap`. When that is its
  only loss, at most one such event is stored per minute, and it reports every
  drop since the previous one.
- An unmetered 401, 403, or TLS rejection never persists. See
  [Find authentication failures](#find-authentication-failures).

A panic also produces one ERROR event with `event_type=panic` on the target
`trawl_server::panic`. It carries the source `file`, `line`, `column`, and the
`thread` name, never the panic message. It goes to stdout, and to `log_file`
when file logging is active. It never persists. The failure event of the caught request is the stored
record, with `stage=panicked`.

When trawld runs its terminal monitor, which is when you start it without
`--no-monitor`, stdout carries no log lines. If trawld also writes no
`log_file`, nothing records the panic event. In that case trawld also writes
one line to stderr, with the location and no message:

```text
trawld: panicked at FILE:LINE:COLUMN on thread 'NAME'
```

## Restore missing log lines

Symptom: expected `trawl_server` or `fleet_auth` lines are absent from the
journal or from stored telemetry.

Check:

1. Look for a parse warning.

   ```bash
   journalctl -u trawld | grep config_warning
   ```

   `RUST_LOG is set but could not be parsed` means trawld ignored the value and
   used its default. It does not log the raw value.

2. Compare the current `RUST_LOG` against the default:

   ```text
   trawl_server=info,trawld=info,fleet_auth=info,auth.backend=info,storage.backend=info,preauth.transport=info
   ```

Fix: set a valid `RUST_LOG` that keeps these six targets, then restart trawld.
On Debian the variable lives in `/etc/default/trawld`. On Helm it is `logLevel`.
The same filter feeds stdout and
[`internal_telemetry`](/reference/configuration/#ingest).

| Target | Diagnostics |
| --- | --- |
| `trawl_server` | Handlers, ingestion, and compaction |
| `trawld` | Startup, configuration warnings, and task panics |
| `fleet_auth` | Authentication middleware |
| `auth.backend` | Authentication database failures |
| `storage.backend` | App-state database failures |
| `preauth.transport` | TLS handshakes and connection failures |

A global `info` filter also enables dependency logs.

The browser proxy reads its own `RUST_LOG`. Its default is
`trawl_web=info,fleet_auth=info`, set in `/etc/default/trawl-web` on Debian and
`web.logLevel` on Helm. A filter copied from trawld omits `trawl_web` and
silences the proxy's session, origin, and upstream diagnostics.

A configuration-file failure happens before tracing starts. It appears on
stderr with the resolved config path and never in stored telemetry.

## Find authentication failures

Symptom: a client reports 401 or 403, and a query for `service=trawld` shows no
rejection event.

Check: events from the targets `fleet_auth`, `auth.backend`,
`preauth.transport`, `trawl_server::policy::unmetered`, and
`trawl_server::panic` never enter stored telemetry, whatever `RUST_LOG` says.
Targets beneath `trawl_server::transport::failure::unmetered` never enter
stored telemetry either. That exact target is the one exception. It persists
under a cap, as [Trace a server failure](#trace-a-server-failure) describes.
The excluded events print to stdout, and to `log_file`
when file logging is configured and either `[ingest] enabled` or
`internal_telemetry` is false. With both enabled, `log_file` is not opened.
Read these events in the daemon output, or read
`trawl_auth_failures_total` on `/metrics`. Its `reason` label has five values.

| `reason` | Meaning | Fix |
| --- | --- | --- |
| `unauthorized` | The bearer token is missing, invalid, or revoked. The client sees 401. | Issue a key or re-enable the key. |
| `no_trawl_grant` | The key is valid but holds no Trawl permission. The client sees 403. | Grant a Trawl role to the key. |
| `forbidden` | The key lacks the permission the route needs. The client sees 403. | Grant the route permission. |
| `backend_unavailable` | The auth database did not answer. | See `auth_db` under [Read the checks](#read-the-checks). |
| `internal` | The middleware failed. | Read the trawld journal. |

The metric carries no key name and no request path. Ship the stdout stream
through your log pipeline when you need those details.

## Enable the query debug log

Use this log when you need the raw DSL, the generated SQL with its parameter
values, the source paths, and sample rows for one query. Each entry names the
authenticated identity. The file can expose more than the event corpus.

1. Set `server.query_log` in `trawld.toml`, or `TRAWL_QUERY_LOG`, or pass
   `--query-log` to trawld. Point it at a directory only trawld can write, such
   as `/var/lib/trawl`.
2. Restart trawld. The journal shows one `query_log_enabled` warning that names
   the path.
3. Tail the file.

   ```bash
   tail -f /var/lib/trawl/query-debug.log | jq
   ```

trawld creates the file with mode `0600`, tightens an existing looser file, and
refuses a symlink at the path. At `server.query_log_max_bytes`, 100 MiB by
default, it renames the file to `<path>.1` and starts a new one. One rollover
file is kept. `0` disables rollover.

## Remove the query debug log

1. Unset `server.query_log`, `TRAWL_QUERY_LOG`, and `--query-log`.
2. Stop trawld.
3. Delete the log and its `.1` sibling.
4. Start trawld.

Do not delete the active file while trawld runs. trawld keeps writing to the
unlinked inode, the space is not reclaimed, and the next rollover rename fails.
After a failed rename trawld retries after another `query_log_max_bytes` of
output, with one warning per attempt. If a rename succeeds but the reopen fails,
trawld undoes the rename. If the undo also fails, trawld closes the log until
restart.

Default telemetry for queries, exports, and streams carries `query_id`,
`query_len`, the actor, the outcome, timing, and `error_class`, never raw query
text. Raw text is in query history, in this log, and in the DEBUG events
`query_text` and `query_error_text`.
