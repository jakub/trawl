---
title: Connect log senders
description: Send events to trawld over HTTP or syslog and confirm that they arrive.
---

trawld accepts events at `POST /api/v1/ingest` and, when enabled, on a syslog
listener. Every event passes through the same canonicalizer. The
[event contract](/reference/events/) defines the fields, repairs, and rejection
rules. This page connects one sender and confirms the result.

## Send events over HTTP

You need a service key whose role has `trawl:ingest`, from
[Create roles and keys](/operate/access/#create-roles-and-keys), and the trawld
HTTPS address. `trawl-web` answers 404 on `/api/v1/ingest`.

1. Write a test batch. The body is a JSON array or newline-delimited JSON
   objects. `service` is required. `env` must be in `[ingest] envs`, which
   defaults to `["prod"]`:

   ```bash
   printf '[{"service":"ingest-check","host":"app1.example.com","env":"prod","level":"info","timestamp":"%s","message":"connection check"}]\n' \
     "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > events.json
   ```

   The timestamp is the current time, so the `last=1d` query below finds
   the event. An old timestamp is stored as sent and falls outside that window.

2. Send it:

   ```bash
   curl --fail-with-body -H "Authorization: Bearer $(cat vector.token)" \
     -H 'Content-Type: application/json' --data-binary @events.json \
     https://trawl.example.com:5514/api/v1/ingest
   ```

   Expect `{"accepted":1}`. A rejected event adds `rejected` and an `errors`
   array. Each entry gives the event's `index`, its `reason` code from the
   [rejection table](/reference/events/#rejection-reasons), and a `message`.
   Accepted events in the same batch still land.

3. Query it back:

   ```bash
   trawl -p prod query 'service=ingest-check host=app1.example.com last=1d | head 10'
   ```

   Expect one row with `_producer` = `http`, `_severity` derived from `level`,
   and no `_repairs`.

trawld fills or refuses the envelope fields as follows:

| Field | What trawld does |
| --- | --- |
| `service` | Rejects the event when it is missing, empty, or outside the service charset. |
| `env` | Fills `[ingest] default_env` and records `env.defaulted` when missing. Rejects a value that is not in `envs`. |
| `host` | Fills the peer address and records `host.from_peer` when missing. Rejects a missing host when the peer is in `[ingest] trusted_relays`. |
| `_time` | Reads `_time`, `timestamp`, and `@timestamp`, first present. Uses arrival time and records `time.from_ingest` when none parses. |
| `_severity` | Reads `severity`, `severity_text`, and `level`, first mappable. Leaves `_severity` absent when none maps. |

The body limit is `[ingest] max_body_bytes`, 16M by default, and
`Content-Encoding: gzip` is accepted. A gzip body must also fit the limit
once decoded, or trawld answers 413. Each key gets
`[server.rate_limit] ingest_rpm` requests per minute on this route, 1000 by
default. For journald and file logs, follow
[Ship logs with Vector](/getting-started/vector-integration/).

### Keep each batch under the admission ceiling

trawld reserves hot-buffer space for a whole request before it writes it. An
HTTP request may use at most 15/16 of `[ingest] hot_buffer_max_events` and
15/16 of `hot_buffer_max_bytes`. With the defaults, that is 93,750 events and
98,304,000 bytes of canonical ndjson. The byte count is measured after gzip
decoding.

- Size sender batches well under both limits. A larger request answers 413
  `ingest_batch_too_large`, and it can never succeed. Vector drops a batch that
  gets a 413 instead of retrying it. The Vector configuration on this site
  sends 1 MB batches, far below the default limits.
- If you lower the hot-buffer caps, lower the sender's batch size with them.
- Let the sender retry a 503 `hot_buffer_full`. Nothing from that request was
  written, so a retry does not duplicate events. `Retry-After` gives the
  compaction interval in seconds, which is when space can next free.

A 503 that repeats for minutes means compaction is not draining. See
[ingest admission refusing](/operate/operational-alerts/#ingest-admission-refusing).

## Ingest preview

The ingest preview shows what trawld would do to a sample of events, and
stores none of them. Run it on a sample from a new sender before you switch
the sender on. `trawl preview-ingest` sends the sample and prints one row per
event:

```bash
trawl -p prod preview-ingest capture.ndjson
```

The key needs `trawl:server_manage`. A collector's `trawl:ingest` key cannot
preview, because the report shows server configuration: the allowed envs, the
trusted relays, and the derivation sources. The route is
`POST /api/v1/ingest/preview`. See the [API reference](/reference/api/#preview-ingest)
and the [CLI reference](/reference/cli/#preview-ingest-mode). To capture a
sample from Vector, see
[Preview a sample](/getting-started/vector-integration/#preview-a-sample).

### What the preview shows

The preview reads the same body as `/api/v1/ingest`, NDJSON or a JSON array,
and passes it through the same parse and canonicalization step. It covers the
HTTP producer only. Syslog frames have their own parser and are not
previewed. For each event, in input order, the report gives one of two
outcomes:

- **Accepted.** The canonical event as trawld would store it, its repair
  codes, and its lineage. The lineage names the field that gave `_time`, or
  says that the arrival time filled it. It names the field that gave
  `_severity`, the earlier sources that did not map, or says that no source
  mapped. It lists each field that was renamed, dropped, truncated, or
  stringified. A change that is a repair carries its repair code. A
  stringified field carries none.
- **Rejected.** The `reason` and `message` that real ingest gives for the
  same event. A line that is not valid JSON, or an array element that is not
  an object, is a rejected event too. No partial canonical event is shown.

The report also gives the peer address, whether that address is a trusted
relay, the one arrival time that every event in the sample shares, and the
`time_from` and `severity_from` lists in the order trawld reads them. A sample
in which every event is rejected still answers `200`.

### Name the sender's address

trawld fills a missing `host` from the sender's address, or rejects the event
when that address is in `[ingest] trusted_relays`. The machine that runs the
preview is rarely the sender. To get the real outcome for events without
`host`, pass the address that trawld sees for the collector, after any NAT:

```bash
trawl -p prod preview-ingest capture.ndjson --peer-ip 192.0.2.10
```

Without `--peer-ip`, trawld uses `192.0.2.1`, an address reserved for
documentation, and the report says that no peer was given. Each event
without `host` is then marked as depending on the sender, rejected events
included. trawld reads `host` after it normalizes field names, so `Host` and
`_host` count as `host`, and a `host` of `null` counts as no `host`. A line
that is not valid JSON, or an element that is not an object, is never marked.
trawld classifies
the placeholder against its relay configuration like any other address. An
event that carries `host` gets the same outcome with or without a peer. The
shipped Vector configuration sets `host` on every event.

### Bounds

- The body limit is `[server] max_request_body_bytes`, 128K by default, not
  the ingest limit. A larger body answers `413`.
- A sample holds at most 500 events. A line that is not valid JSON counts.
  A blank line does not. A larger sample answers `413 preview_too_large`.
  trawld refuses the whole sample and never truncates it.
- The body must be uncompressed. Any `Content-Encoding` other than
  `identity`, `gzip` included, answers `415 unsupported_encoding`.
- The preview spends the key's interactive rate bucket,
  `[server.rate_limit] default_rpm`, not the ingest bucket.
- On a node with `[ingest] enabled = false`, the route answers `404`. That
  node does not receive events, so its configuration does not describe the
  node that will.

### What the preview leaves behind

The preview stores nothing from the sample. It writes no WAL, takes no
hot-buffer space, and publishes nothing to the hot buffer or to a live
stream. It changes no ingest, repair, rejection, or unmapped-severity counter,
no `/stats` total, and no service label on the repair metric. No log line
carries a sample value.

The request itself leaves the same traces as any other read:

- the key's last-used time;
- one request from the key's interactive rate budget;
- an [`http_failure` event](/operate/health/#trace-a-server-failure) if trawld
  answers with a 5xx.

With debug logging on, trawld also logs the request line: method, path, and
status. None of these traces holds a value from the sample. Every response
from the route carries `Cache-Control: no-store`, because a report quotes the
sample. Refusals carry it too.

### What the preview does not promise

A preview reports canonicalization and nothing more. An accepted event in a
preview does not mean any of these:

- That the batch will be admitted. Hot-buffer capacity changes from moment
  to moment.
- That the collector's key works. The preview runs with your key, not the
  collector's.
- That every value survives compaction. A value whose type conflicts with
  the [field catalog](/operate/catalog/) is shelved at compaction time.
- That the same sample gets the same repairs after the configuration
  changes. A change to `envs`, `default_env`, `trusted_relays`, `time_from`,
  or `severity_from` changes the result.
- That the deployment is correct. The doctors check the deployment, see
  [Check the server](/operate/health/#check-the-server). The
  [sender proof recipes](/getting-started/vector-integration/#prove-the-first-event-arrived)
  prove that a real sender's events arrive.

## Receive syslog

A network appliance, such as a firewall, can send syslog directly to trawld's
listener. This recipe connects one appliance and proves that its first event
arrived, with the same pattern as
[Prove the first event arrived](/getting-started/vector-integration/#prove-the-first-event-arrived).
How trawld parses the fields depends on the vendor's line format. trawld
reads the syslog frame. It does not parse fields inside the vendor's message
text, such as a rule name, and those stay in `message`.

1. Enable the listener in `/etc/trawl/trawld.toml`. Set `allow_cidrs` to the
   appliance's address, and map that address to a service name in
   `source_service_map`:

   <!-- proof:syslog-config -->
   ```toml
   [syslog]
   enabled = true
   udp_addr = "0.0.0.0:1514"
   tcp_addr = "0.0.0.0:1514"
   allow_cidrs = ["192.0.2.1/32"]
   default_service = "syslog"

   [syslog.source_service_map]
   "192.0.2.1" = "firewall"
   ```

   An empty `allow_cidrs` accepts every peer. `allow_cidrs` is not
   authentication. A UDP sender can forge its source address, and the
   listener has no TLS and no tokens. Anyone who can reach the port can send
   events that look like the appliance's. Keep `allow_cidrs` as narrow as the
   appliance's address, and limit who can reach the port.

   `unifi-syslog.toml`, the Vector drop-in for UniFi devices, also listens on
   port 1514 by default. Do not run it and trawld's listener on the same host
   with the same port.

2. Restart trawld. On Helm, set `config.syslog.enabled: true` and
   `service.syslog.enabled: true` to expose the ports.

3. On the trawld host, set the appliance's address and record the start time
   in UTC:

   <!-- proof:syslog-vars -->
   ```bash
   DEVICE=192.0.2.1
   T0="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
   ```

4. If UFW runs on the trawld host, allow the appliance's syslog through it:

   <!-- proof:syslog-firewall-allow -->
   ```bash
   sudo ufw allow proto udp from "$DEVICE" to any port 1514
   ```

   If the appliance sends over TCP, allow `proto tcp` as well.

5. In the appliance's settings, point remote syslog at the trawld host on port
   1514. Then send the appliance's test message. If it has no test message,
   send a packet that one of its rules blocks and logs.

6. Query for the appliance's events:

   <!-- proof:syslog-check -->
   ```bash
   trawl -p prod query "service=firewall _producer=syslog syslog_source_ip=$DEVICE last=1d _ingested>=\"$T0\" | head 20 | table _time, _ingested, _producer, env, service, host, syslog_source_ip, syslog_severity, syslog_timestamp, syslog_facility, message"
   ```

   The check passes on at least one row. `last=1d` bounds the scan, and
   `_ingested` proves that the event arrived after `T0`. The appliance's test
   message carries no marker, so `syslog_source_ip` and `_ingested` identify
   it. Expect `_producer` = `syslog`, `host` from the frame, and the parsed
   `syslog_severity`, `syslog_timestamp`, and `syslog_facility` columns. `env`
   is `[ingest] default_env`. A sender in `source_service_map` gets that
   service name whatever its APP-NAME says. An APP-NAME that fails the service
   charset falls back to `default_service` with the repair
   `service.from_profile`. The [`[syslog]` reference](/reference/configuration/#syslog)
   lists every key.

### Syslog delivery under load

When the hot buffer has no room, the syslog batcher keeps its pending events
in arrival order and stops taking frames from the listener queue. It tries
again when compaction frees space.

- **TCP** stalls instead of dropping. The listener waits for queue space and
  stops reading the socket, so the kernel's flow control slows the sender. The
  connection stays open; trawld does not close it because the queue is full.
  `tcp_idle_timeout_secs` counts only time spent waiting for data from the
  sender, not time spent waiting for space.
- **UDP** has no flow control. A datagram that finds the queue full is dropped
  and counted in `trawl_syslog_events_dropped_total`. The `reason` label is
  `backpressure` when the batcher was waiting for hot-buffer space, and
  `queue_full` when the batcher was only slow.
- A single event larger than the syslog share of the hot buffer can never be
  admitted. It is dropped and counted as an `oversized` refusal in
  `trawl_hot_buffer_admission_refusals_total{producer="syslog"}`.
- At shutdown, the batcher makes one last attempt. Events that still do not
  fit are dropped and counted with `reason="backpressure"`.
- A TCP frame that is still waiting for queue space at shutdown is dropped
  and counted in `trawl_syslog_events_dropped_total`. The `reason` label
  follows the UDP rule: `backpressure` when the batcher was waiting for
  hot-buffer space, `queue_full` otherwise.

## Map a raw syslog severity sent over HTTP

A collector that forwards the syslog PRI severity, 0 to 7, as a number over
HTTP needs a declared dialect, because a bare number reads as OpenTelemetry 1
to 24. Put the source before any other severity field the collector sends:

```toml
[ingest]
severity_from = [{ field = "syslog_severity", dialect = "syslog" }, "severity", "severity_text", "level"]
```

Restart trawld. The first source that maps wins, `syslog_severity` stays
stored as sent, and `_producer` stays `http`. The change applies to new events
only.

## Confirm delivery over time

Use the [operational alert pack](/operate/operational-alerts/) to distinguish
queue or WAL abandonment, HTTP persistence rejection, and uncertain task
outcomes. An uncertain write may already have durable bytes. Preserve sender
copies and reconcile accepted output before a controlled resend.

- Read the ingest counters on `/metrics`: `trawl_ingest_events_total`,
  `trawl_ingest_events_rejected_total{reason}`,
  `trawl_ingest_repairs_total{code,service}`, and
  `trawl_severity_unmapped_total{service}`. An unmappable severity is counted,
  not repaired.
- Find senders that need a repair:

  ```bash
  trawl -p prod query 'last=1h | stats count() by service, _repairs'
  ```

- Check the collector's retry and buffer state. A `200` with `rejected`
  events counts as success on the collector side.
- If a field arrives but reads as NULL, its type conflicts with the catalog.
  See [Diagnose catalog conflicts](/operate/catalog/).
