---
title: Check health and stalled work
description: Confirm that a trawld server is serving, read what is degraded, read disk headroom and retention reach, and free capacity that a stalled query still holds.
---

These commands address one server. `TRAWL_URL` names its HTTPS API,
`TRAWL_PROFILE` names the matching CLI profile, and `TRAWL_CURL_CONFIG` names
an owner-only curl config file that holds the bearer header.
[Keep a key in a curl config file](/operate/access/#keep-a-key-in-a-curl-config-file)
shows how to write it. Keep the token in that file, not on the command line,
and do not print the file. Server stats and the dashboard need
`trawl:server_manage`. The `trawl query` commands and the list of running
queries need `trawl:query`. Use a key with both the `trawl-reader` and
`trawl-operator` roles, such as `alice-ops` from
[Create roles and keys](/operate/access/#create-roles-and-keys).

The recipes on this page that search trawld's own telemetry filter on
`service=trawld _producer=trawld`, not on `service=trawld` alone. Any HTTP
sender can set `service=trawld` on the events it sends. Only trawld stamps
`_producer=trawld`, so that filter keeps a forged event out of the answer. See
the [event reference](/reference/events/#declared-fields).

## Check the server

To check an installation from the server host, before or after the first start, run
[`trawld --doctor`](/reference/configuration/#check-the-installation-with-trawld---doctor)
as the service user. It checks the databases, the data root, the certificate,
and the listener without changing anything. To check a running server from a
client, run `trawl doctor` (see the [CLI reference](/reference/cli/#doctor-mode)).

For warnings about reported discards, persistence failures, compaction
failures, and quarantined files, use [Respond to operational alerts](/operate/operational-alerts/).
Those rules complement serving checks; they do not detect silent stalls or
establish recovery when an alert resolves.

1. Ask for health. `/api/v1/health` needs no key.

   ```bash
   curl --fail-with-body --config "$TRAWL_CURL_CONFIG" "$TRAWL_URL/api/v1/health"
   ```

   A serving server answers 200 with `"status":"ok"` and six checks that read `ok`:

   ```json
   {"status":"ok","checks":{"duckdb":"ok","auth_db":"ok","storage_db":"ok","data_path":"ok","ingest_capacity":"ok","corpus":"ok"},"version":"..."}
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

| Check | Meaning when it fails | What to do |
| --- | --- | --- |
| `duckdb` | The query engine did not answer its probe. Every query fails. | Read the trawld journal for engine errors. Restart trawld if the engine does not recover. |
| `auth_db` | The auth database did not answer within the probe timeout. Bearer checks fail and `trawl_auth_failures_total{reason="backend_unavailable"}` rises. | Check the auth database and the `[auth]` settings in `trawld.toml`. |
| `storage_db` | The app-state database did not answer. Saved queries, history, and repin jobs fail. | Check the app-state database and the `[storage]` settings. |
| `data_path` | `[data] path` is missing or is not a readable directory. Cold data is unreadable. | Check the mount and the directory permissions for the `trawl` user. |
| `ingest_capacity` | Reads `refusing`, not `error`. The hot buffer is full, so ingest is refused until compaction drains it. Reads stay complete. | Follow [ingest admission refusing](/operate/operational-alerts/#ingest-admission-refusing). |
| `corpus` | Reads `restart_backlog` or `rollup_pending`, not `error`. The server cannot yet count every stored event once, so searches, exports, manual runs, and repins answer 503 `corpus_recovering`. Live tail still works. `restart_backlog` means WAL from before a restart is not yet proven covered. `rollup_pending` means a daily rollup marker is unfinished. | Wait for compaction to clear it. If it stays, follow [corpus unsettled](/operate/operational-alerts/#corpus-unsettled). |

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

## Read disk and retention

The **Disk and retention** section of **Health** answers one question: will
the data disk hold the retention you configured if the recent days repeat? It
needs a key with `server_manage`, like the storage readings. The same data is
the `capacity` object of the [dashboard snapshot](/reference/api/#capacity).

The section has three parts:

- **Headroom**: the free space on each filesystem that trawld writes to, and
  the deletion floor on the data filesystem.
- **Pressure-deletion evidence**: removal counters, pressure attempts, the last
  sweep, and each environment's oldest date.
- **Retention reach**: for each environment, how many days of its
  `max_age_days` the disk is projected to hold.

[Manage retention](/operate/retention/#read-headroom) explains how to read
headroom and the evidence. This section explains reach.

The section colours no value as good or bad and shows no "safe" state. Reach
is a projection from seven days of data, and a green badge would read as a
guarantee. Compare the projected days with the policy you need.

### Read retention reach

A reach of "about 38–52 of 90 days" means: if the observed days repeat,
pressure deletion is projected to leave this environment between 38 and 52
days of its 90-day policy. It is a conditional projection, not a guarantee.

trawld computes reach from the Parquet already on disk:

- **Observed days** are the UTC dates from 8 days ago through 2 days ago.
  Today and yesterday are left out while rollup and late events settle them.
  The section shows the dates it used.
- A date in that window with no Parquet counts as a quiet day of 0 bytes, if
  it is newer than the environment's oldest date. A date older than the oldest
  date is not an observed day.
- The oldest date is the oldest date directory that holds Parquet. An empty
  date directory does not count.
- The **budget** is the Parquet bytes in every environment's date directories,
  plus the available bytes on the data filesystem, minus the floor.
- Pressure deletion removes the dates closest to their expiry first. Under
  sustained pressure, every environment with a finite `max_age_days` keeps
  the same fraction of its policy. trawld solves for that fraction.
- The low end uses the environment's largest observed day. The high end uses
  its mean observed day.

Each end of the range reads in one of three ways:

| Reading | Meaning |
| --- | --- |
| A number of days | The disk is projected to hold this many whole days of the policy. |
| The full policy | The projected fraction is 1 or more. The environment keeps every day of its policy. |
| The disk fills before retention is reached | `min_free_disk_bytes` is 0, so pressure deletion is off. The full policy does not fit in the stored bytes plus the available bytes. No date is given. |

The two ends can read differently. For example, the full policy fits at the
mean observed day but not at the largest.

An environment with `max_age_days = 0` keeps its data forever. It shows its
stored bytes and its mean observed daily growth, and makes no time claim.
Pressure deletion removes its dates last, so the projection keeps all of its
stored bytes. Its growth takes space from the other environments, so their
reach shrinks over time even when their own volume stays the same.

### Read an environment that is excluded from growth

An environment with a finite `max_age_days` and fewer than 3 observed days has
no daily rate. This happens to:

- a new environment, until about four days after its first stored date;
- an environment with `max_age_days` of 3 or less, which never has 3 observed
  days;
- an environment that pressure deletion has cut down to fewer than 3 observed
  days.

Its reach is withheld with the reason `insufficient_history`. The projection
reserves its stored bytes, as it does for a keep-forever environment, and the
section names it as excluded from growth. The other environments still get a
projection. Their projection does not include the excluded environment's
growth, so for a few days after a new environment appears, the others read
higher than they will be.

### Act on a withheld reach

A withheld reach shows a reason and no number.

| Reason | Meaning | What to do |
| --- | --- | --- |
| `insufficient_history` | The environment has fewer than 3 observed days. A keep-forever environment gets this reason too. | Wait for a new environment to collect observed days. For a short policy, read headroom instead. If pressure deletion cut the environment down, add space. |
| `retention_suppressed` | A repin job ran during the Parquet scan or the headroom sample, or repin staging is still on the data root. A repin holds two generations of files, so the sample can overstate stored bytes or understate free space. This holds for a job that was cancelled, or refused after it built its shadow, too. A dry run or a refusal at the scan gate writes no second generation and does not suppress. Every environment gets this reason. | Wait for the repin to finish and for the next samples. If the reason stays, follow [Clear suppressed retention](/operate/retention/#clear-suppressed-retention). |
| `measurement_unavailable` | The Parquet measurement or the headroom measurement is not `complete`. Every environment gets this reason. A retained, failed sample never feeds a projection. The raw readings stay visible with their age. | After a start, wait for the first measurements. If a measurement failed, check the data path as described in [Read the checks](#read-the-checks). |

When more than one reason applies, `measurement_unavailable` wins, then
`retention_suppressed`, then `insufficient_history`.

### Why there is no countdown

With a floor set, the data disk does not fill. When free space falls below the
floor, pressure deletion removes the dates closest to their expiry. The loss
lands on retention: an environment set to 90 days keeps 40. A "days until
full" countdown would count down to an event that does not happen.

Free space also misleads under pressure. While pressure deletion removes data,
free space stays flat near the floor, so a trend on it reads as stable while
history is lost. Reach states the cost in the unit you configured: days kept.

### Read the terminal summary line

In the terminal's **DATA PIPELINE** panel, the third line summarizes the data
filesystem and retention:

```text
free: age 2s 120.0 GB/500.0 GB  floor 1.0 GB  sweep: completed 12m 34s ago  pressure deletions: 3
```

- `free:` shows the headroom status and age, then the data filesystem's
  available and total bytes. `failed age 45s` marks a retained sample.
  `awaiting measurement`, `not configured`, and `failed; unavailable` carry no
  bytes.
- `floor 1.0 GB` reads `pressure deletion off` when `min_free_disk_bytes` is 0.
- `sweep:` shows the last sweep's outcome and age, or `none yet`.
- `pressure deletions:` counts `disk_pressure` removals since process start.

At 80 columns only the `free:` part and the floor fit. The sweep and the count
appear from about 140 columns. The line never shows an environment's reach,
because one environment's reach is not the answer for the installation. The
**Dashboard** tab of the `trawl` terminal UI shows the same line to a key with
`server_manage`.

## Diagnose a 503 or 504 from a query

`timeout_secs`, 30 seconds by default, starts one deadline right after
authentication, and that deadline covers admission, queueing, and execution.
Where it expires decides the status code. Two other 503s refuse a query
whose answer could not be complete.

| Symptom | Meaning | What to do |
| --- | --- | --- |
| 503 with `server at capacity: the query was not started` | The deadline expired before any database work started. Nothing ran, and query history has no row. | Read `pool_retained` and the `retained` list. Cancel retained work, or raise `max_concurrent_queries`. |
| 504 with `query timed out` | The deadline expired after work started. The bind or scan can still hold its permit. | Find the id in `retained` and cancel it. |
| `trawl_query_permits_retained` stays above zero | Finished requests still occupy the pool. | Cancel each retained id. |
| 503 `corpus_recovering` | The corpus is unsettled after a restart or an interrupted rollup. Nothing ran. | Read `checks.corpus` in `/api/v1/health`. If it stays unsettled, follow [corpus unsettled](/operate/operational-alerts/#corpus-unsettled). |
| 503 with `recent events could not be read for this query` | The hot buffer's snapshot could not be built, so the server refused the query rather than answer without the newest events. | Read the `http_failure` event's `cause_kind`, then the trawld journal for the I/O error. See [Trace a server failure](#trace-a-server-failure). |

To cancel, send `DELETE /api/v1/queries/{id}` with `server_manage`, or with
`query_cancel` when your key submitted the query. The response
`{"cancelled":true,"query_id":N}` acknowledges the request without proving that
the work stopped, and a repeated cancel changes nothing. trawld records a cancel
that arrives during the bind and checks it again before execution. A bind
already inside DuckDB cannot be interrupted, so its permit returns only when the
bind returns.

## Find the slow phase of a query

Symptom: a query is slow or timed out, and you need to know where its time went.

trawld writes one `query_timing` event for each DSL query that reaches its DSL
check. That covers interactive queries, `from saved` queries, exports, and
scheduled and manual report runs, including the ones refused at the DSL check
or for capacity. Live tail, the pool ping, and field sampling write none. The
event splits the query's time into phases. Each phase is an integer number of
microseconds in a field named `query_<phase>_us`, such as `query_bind_us`.
[Query timing](/architecture/reports-telemetry/#query-timing) lists the phases
and the other fields.

1. Read the `X-Request-Id` header of the slow response, and find its timing
   event.

   ```bash
   trawl -p "$TRAWL_PROFILE" query 'service=trawld _producer=trawld event_type=query_timing request_id=01K5EXAMPLE0000000000000000 last=24h | table _time, query_id, kind, outcome, timing_complete, query_observed_us, query_pool_wait_us, query_source_us, query_bind_us, query_execute_us, query_other_us'
   ```

   If you know the query ID instead, from `/api/v1/queries` or an
   `http_failure` event, filter on it. Keep the time bound, because query IDs
   restart at 0 each time trawld starts.

   ```bash
   trawl -p "$TRAWL_PROFILE" query 'service=trawld _producer=trawld event_type=query_timing query_id=4182 last=1h | table query_bind_us, query_execute_us, query_pool_wait_us'
   ```

   A scheduled or manual run has no request. Its events carry `run_id`, the
   `id` of its [report run](/reference/api/#report-runs), instead of
   `request_id`.

2. Compare the phases. The largest one is where the query spent its time.

   - An absent field means the query never entered that phase. A query without
     `from saved` has no `query_saved_lookup_us`, and only a `timechart` query
     has `query_probe_us`.
   - A present `0` is a measurement: the phase ran for less than one
     microsecond.
   - `query_observed_us` is the whole window. It starts at the same instant as
     the response's `execution.duration_ms`. An export has no
     `execution.duration_ms`: its window starts before `dsl_check`, earlier
     than `export_complete.duration_ms` starts, and runs through `render`.
   - `query_other_us` is the observed time that no phase measured. On a
     complete event, the present phases plus `query_other_us` add up exactly to
     `query_observed_us`. A large `query_other_us` is a gap in trawld's
     measurement, so report it.
   - `duckdb_attempts` counts how many times DuckDB bound the main statement.
     When it is more than 1, `fallback` says why: `raw_retry`, `hot_only`, or
     `both`. `query_bind_us` and `query_execute_us` add up every attempt.

3. To see which phase is slow across many queries, aggregate the phases.

   ```bash
   trawl -p "$TRAWL_PROFILE" query 'service=trawld _producer=trawld event_type=query_timing outcome=success last=24h | stats count(), p95(query_observed_us), p95(query_pool_wait_us), p95(query_source_us), p95(query_hot_snapshot_us), p95(query_bind_us), p95(query_execute_us), p95(query_other_us) by kind'
   ```

   A high `query_pool_wait_us` means queries waited for an executor permit.
   See [Inspect capacity](#inspect-capacity). A high `query_bind_us` or
   `query_execute_us` means DuckDB itself was slow for that query shape.

`outcome` is `success`, `error`, `capacity_refused`, `timeout`, or `abandoned`.
An `error` event carries `error_class`, never the error text. A
`capacity_refused` event has `work_started=false` and no worker phase. It
carries the wait that ran out: `query_pool_wait_us` or
`query_publication_wait_us`, or `query_saved_lookup_us` when the deadline cut a
`from saved` lookup before the pool was reached. It is the 503 in
[Diagnose a 503 or 504 from a query](#diagnose-a-503-or-504-from-a-query).
`abandoned` with `work_started=false` means the client went away before any
work started, and nothing is left to report.

### Join a timed-out query to its final totals

When a query times out, or its client goes away while its work runs, trawld
writes `query_timing` at once. It does not wait for the worker. That event
carries `timing_complete=false`, and it is partial:

- It carries the phases that finished. The time of the unfinished iteration is
  not booked. A phase with no earlier completed iteration has no
  `query_<phase>_us` field. A phase that also ran to completion earlier keeps
  those earlier iterations in its field. For example, on the second bind of a
  `raw_retry`, `query_bind_us` holds the first bind only.
- `active_query_phase` names the phase still running, and `active_elapsed_us`
  says how long its current iteration had run.
- `query_observed_us` equals the finished phases, plus `active_elapsed_us`,
  plus `query_other_us`.

When the worker finishes and returns its permit, trawld writes
`query_permit_reclaimed` with the same `query_id`, and the same `request_id` or
`run_id`. That event carries the worker's final phase totals,
`duckdb_attempts`, `fallback`, and `physical_outcome`: `completed`, `failed`,
`cancelled`, `panicked`, or `not_started`. A `not_started` reclaim follows a
capacity refusal where trawld had handed the work to a worker, but the deadline
expired before the work started. It carries no phases, because no work ran. A worker that never finishes writes no reclaim event.

If the worker had already finished and released its permit when the deadline
expired, no work is left running. trawld then writes one complete event with `outcome=timeout` and
`timing_complete=true`, and no reclaim follows.

Join the two events on `request_id`, or on `run_id` for a report run. A
`query_id` alone can match a query from before a restart.

```bash
trawl -p "$TRAWL_PROFILE" query 'service=trawld _producer=trawld event_type=query_timing,query_permit_reclaimed request_id=01K5EXAMPLE0000000000000000 last=24h | table _time, event_type, query_id, outcome, timing_complete, active_query_phase, active_elapsed_us, physical_outcome, retained_ms, query_bind_us, query_execute_us'
```

To see which phase timeouts stop in, count the partial events.

```bash
trawl -p "$TRAWL_PROFILE" query 'service=trawld _producer=trawld event_type=query_timing timing_complete=false last=24h | stats count() by outcome, active_query_phase'
```

Timing events are best-effort telemetry. When the telemetry buffer is full,
trawld drops them and counts the loss in
`trawl_telemetry_events_dropped_total`. A missing `query_timing` event does not
prove that the query did not run.

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
| `cause_kind` | A closed kind taken from the typed error beneath the class: an I/O error kind such as `io_storage_full`, a DuckDB kind such as `duckdb_failure`, a Postgres kind such as `pg_pool_timed_out`, `auth_worker`, `hot_buffer_full` for an ingest request that the hot buffer had no room for, or `restart_backlog` or `rollup_pending` for a read refused as `corpus_recovering`. `unknown` when a server fault kept no typed source: an `internal` error or a `service_unavailable` other than a capacity refusal, including the 500 and 503 that the authentication layer answers, such as the auth backend being down. `none` when the class is the whole cause, as for `timeout`, `panic`, or a capacity refusal, and when nothing was recorded. |
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
   trawl -p "$TRAWL_PROFILE" query 'service=trawld _producer=trawld request_id=01K5EXAMPLE0000000000000000 last=24h | table _time, event_type, _severity, route, stage, reached, error_class, cause_kind, query_id'
   ```

3. If the failure event carries a `query_id`, find the query lifecycle events
   for that ID. `query_failed` names the query ID and its own `error_class`.

   ```bash
   trawl -p "$TRAWL_PROFILE" query 'service=trawld _producer=trawld query_id=4182 last=24h | table _time, event_type, _severity, error_class, duration_ms'
   ```

4. For a failure with `stage=unrecorded`, read `reached`. The producer that
   sent the 5xx without recording why sits after that point, and that
   producer is the bug to report.

Not every failure event persists:

- A metered failure event has no rate cap of its own. The per-key rate limiter
  already bounds how many of them one client can cause. Like every
  self-telemetry event, it persists only if the log filter keeps it and the
  telemetry buffer budget has room for it.
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

trawld runs its terminal monitor when stdout is a terminal and you start it
without `--no-monitor`. The monitor owns stdout, so stdout carries no log
lines. If trawld also writes no `log_file`, nothing records the panic event.
Nothing records it either when `RUST_LOG` filters out the
`trawl_server::panic` target, for example `RUST_LOG=trawld=debug`. In either
case trawld also writes one line to stderr, with the location and no message:

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

Symptom: a client reports 401 or 403, and a query for `service=trawld _producer=trawld`
shows no rejection event.

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

trawld and `trawl-web` colour their stdout lines only on a terminal, and never
when `NO_COLOR` is set, so the journal and `kubectl logs` hold plain text. When
a collector ships the trawld journal back into the same Trawl, as the
[Debian Vector configuration](/getting-started/vector-integration/) does, each
line that stored telemetry also keeps arrives twice: once with
`_producer=trawld` and once with `_producer=http`. Query
`service=trawld _producer=trawld` for stored telemetry, and
`service=trawld _producer=http` for the stdout-only events above.

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
