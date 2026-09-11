---
title: Check health and stalled work
description: Confirm that a trawld server is serving, read what is degraded, and free capacity that a stalled query still holds.
---

These commands address one server. `TRAWL_URL` names its HTTPS API,
`TRAWL_PROFILE` names the matching CLI profile, and `TRAWL_CURL_CONFIG` names
an owner-only curl config file that holds the bearer header. Keep the token in
that file, not on the command line, and do not print the file.

## Check the server

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

Check: rejections from the targets `fleet_auth`, `auth.backend`,
`preauth.transport`, and `trawl_server::policy::unmetered` never enter stored
telemetry, whatever `RUST_LOG` says. They print to stdout, and to `log_file`
when telemetry is disabled. Read them there, or read
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
