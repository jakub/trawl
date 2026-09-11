---
title: Check health and stalled work
description: Distinguish dependency health, authorization failures, and workers that outlive a query.
---

Start with the named server and its existing TLS trust and credential configuration.
For curl, keep the bearer header in an owner-only config file, not a command-line
argument. These examples assume `TRAWL_URL` names the direct HTTPS API,
`TRAWL_CURL_CONFIG` names that file, and `TRAWL_PROFILE` names the same server.
Do not print the config file. Disable shell tracing when loading credentials.

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/health"
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/whoami"
trawl -p "$TRAWL_PROFILE" query 'last=15m | head 20'
```

## Read the checks

`/health` is unauthenticated. An auth database, storage database, or data-path
failure can report `degraded` with HTTP 200; an unavailable query engine reports
503. Read every check. Then verify authenticated identity and the permissions
needed for the failing operation. A 401 points at credential validity; a 403
can mean a valid key lacks any recognized Trawl permission or the route permission.

Use [the API reference](/reference/api/#health) for the response shape.
For a packaged host, inspect `systemctl status trawld trawl-web` and their journals.
For Kubernetes, inspect the selected context, namespace, pod events, and each
container separately, including `init-auth`. Avoid dumping environment variables
or Secret contents into incident logs.

## Inspect capacity

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/stats"
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/queries"
```

### The query deadline, and work that outlives a request

`timeout_secs` sets one deadline immediately after authentication. It covers DSL
admission, saved-source resolution, executor queueing, the publication gate, and
execution. Receiving a permit does not reset it; a queued worker can still reach
its start boundary after the deadline and be refused.

A best-effort history write runs under it too, so a slow store can cost the history row
but never the answer that is already in hand. Time spent reading the request body is
outside it, and so is delivering the response.

Where the deadline expires decides the status code:

- Before the work starts, the answer is `503` with
  `server at capacity: the query was not started`. That is a fixed
  sentence, and it is the whole answer: nothing was read, nothing ran,
  and no timeout is written to query history.
- After the work starts, the answer is `504`, a query timeout.

The work-start transition is the boundary, not the order two timers
happen to fire in. Holding an executor permit is not the same as having
started: a worker can sit in the queue holding nothing, or hold a permit
and be refused at the transition because the deadline passed while it
waited.

A pre-start `503` means no database work started for that request. A `504`
means the request ended after work started, including a schema value
sample. The DuckDB bind or scan can continue and keeps its executor permit
until it physically finishes. trawld reports that retained capacity:

- `GET /api/v1/queries` carries a `retained` list beside the active one.
  Each entry has the pool `id`, the work `kind` (`query`, `from_saved`,
  `export`, `scheduled`, `ping`, `sample`), whether it `started`, and
  `retained_ms`, how long it has outlived its request. A query some key
  submitted also carries that key's display name and its DSL, to every
  reader holding `query`, the same metadata the `active` and `recent`
  lists carry for the same query. An autocomplete `sample` is owned by
  the key that asked for it too, so its entry carries that key's display
  name to any reader holding `query`, and never any query text: a sample
  is a field lookup, not DSL. Only work with no owner at all, `ping` and
  `scheduled`, hides its name and its text from anyone below
  `server_manage`. Reading an entry is not authority to stop it:
  cancellation needs `server_manage`, or `query_cancel` held by the exact submitting key.
- `GET /api/v1/stats` and the dashboard snapshot carry `pool_retained`
  beside `pool_active`. Retained work is a subset of held permits,
  never an extra count, and the terminal dashboard renders
  `active: 3/4 (1 retained)` only when the number is nonzero.
- `/metrics` carries `trawl_query_permits_retained`, a label-free gauge.
  A steady nonzero value means capacity is occupied by work no request is
  waiting for any more, and that is the number to alarm on if searches
  start queueing behind nothing visible.
- The lifecycle logs `query_permit_retained` and `query_permit_reclaimed`
  bracket each interval. They carry metadata only, never DSL.

`DELETE /api/v1/queries/{id}` still works on retained
work, and repeating it is safe: cancellation is a latch, and asking twice
sets a flag that is already set. What comes back is an acknowledgement
that cancellation was requested, not a promise that anything has
stopped. trawld latches the request even before an interrupt handle
exists, so a cancel that arrives during binding is not lost, and it
checks the latch again at the boundary between binding and execution.
A bind already inside DuckDB is not preemptible: the honest worst case is
that the permit stays retained until that bind returns.

The DSL admission limits are fixed. The 512 alias-expansion
budget and the 128-stage cap ([DSL reference](/reference/dsl/)) are fixed
constants, checked before a query reaches the database, and there is no
knob here that raises them.

## Logging filter (`RUST_LOG`)

trawld resolves one directive string at startup for stdout logging and
[`internal_telemetry`](/reference/configuration/#ingest):

- If `RUST_LOG` is unset, it uses the packaged default below.
- If `RUST_LOG` is valid, that value replaces the default.
- If `RUST_LOG` is invalid, it uses the default and emits one `config_warning`
  about the parse error. It does not log the raw environment value.

```text
trawl_server=info,trawld=info,fleet_auth=info,auth.backend=info,storage.backend=info,preauth.transport=info
```

The code fallback, Helm `logLevel`, and Debian environment example use this
same default. Keep these targets when customizing the filter:

| Target | Diagnostics |
| --- | --- |
| `trawl_server` | Handlers, ingestion, and compaction |
| `trawld` | Startup, configuration warnings, and task panics |
| `fleet_auth` | Authentication middleware |
| `auth.backend` | Authentication database failures |
| `storage.backend` | App-state database failures |
| `preauth.transport` | TLS handshakes and connection failures |

A global `info` filter also enables dependency logs. A configuration-file
failure happens before tracing starts and appears on stderr with the resolved
config path; it cannot appear in stored telemetry.

Some rejection events remain outside the stored event corpus, regardless of
`RUST_LOG`. The excluded targets are `fleet_auth`, `auth.backend`,
`preauth.transport`, and `trawl_server::policy::unmetered`. Their events still
print to stdout, and to `log_file` when telemetry is disabled.

These rejections occur before per-key rate limiting. For example, a client can
provoke a TLS handshake warning without authenticating, and a valid key with no
Trawl permission receives a 403 before it reaches a rate bucket. Persisting each
rejection would let that traffic fill the data filesystem.

Monitor those failures through the external log pipeline and the
`trawl_auth_failures_total` metric. Its `reason` label has five possible values:
`unauthorized`, `backend_unavailable`, `no_trawl_grant`, `forbidden`, and `internal`.
It carries no key, display name, or request path. Events emitted behind the rate
limiter, `storage.backend` errors, and catalog/health events remain eligible for
stored telemetry.

The browser proxy uses a separate filter. Its default is:

```text
trawl_web=info,fleet_auth=info
```

`trawl_web` carries session, origin, upstream, and startup diagnostics. `fleet_auth`
carries the shared session and origin checks. The proxy's code fallback, Helm
`web.logLevel`, and Debian environment example agree. A valid `RUST_LOG` replaces
that default. Copying trawld's target filter would omit `trawl_web` and silence
those proxy diagnostics.

## The query debug log

Set `server.query_log`, `TRAWL_QUERY_LOG`, or `--query-log` to enable an ndjson
log with one entry per query execution. Each entry includes authenticated identity,
raw DSL, generated SQL and parameter values, source paths, hot-buffer state, and
sample result rows. This log can expose more than the event corpus itself.

Use a directory only trawld can write, such as `/var/lib/trawl`, and plan a
restart to enable or disable logging. File modes cannot protect a directory
entry that another local user can replace.

When opening the log, trawld:

- creates it with Unix mode `0600` and tightens an existing looser file;
- refuses a symlink at the configured path through `O_NOFOLLOW`;
- emits a startup warning naming the path and describing the sensitive contents;
- limits it with `server.query_log_max_bytes`, 100 MiB by default, and retains
  one rollover file at `<path>.1`, also mode `0600`. Zero disables rollover.

There is no age-based cleanup or additional generation count. Once debugging is
finished, disable the log, stop trawld in a planned window, remove both files,
and restart. Deleting the active file while trawld runs leaves it writing to an
unlinked inode, so space is not reclaimed until it closes the file. That also
breaks the rollover rename.

A failed rename is retried after another `query_log_max_bytes` of output, with
one warning per attempt. If the rename succeeds but reopening fails, trawld tries
to undo the rename. If that undo also fails, it closes the log and emits an error.
Logging then remains disabled until restart rather than growing a file that the
configured path no longer names.

Default `service=trawld` telemetry for queries, exports, and SSE streams contains
query metadata: `query_id`, `query_len`, actor, outcome, timing, and `error_class`.
It contains neither raw query/error text nor result samples. Raw text is available
in authenticated query history, the opt-in debug log, and DEBUG tracing events
`query_text` and `query_error_text`.
