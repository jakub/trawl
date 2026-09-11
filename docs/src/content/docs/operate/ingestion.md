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
   cat > events.json <<'JSON'
   [{"service":"ingest-check","host":"app1.example.com","env":"prod","level":"info",
     "timestamp":"2026-09-11T12:00:00Z","message":"connection check"}]
   JSON
   ```

2. Send it:

   ```bash
   curl --fail-with-body -H "Authorization: Bearer $(cat vector.token)" \
     -H 'Content-Type: application/json' --data-binary @events.json \
     https://trawl.example.com:5514/api/v1/ingest
   ```

   Expect `{"accepted":1}`. A rejected event adds `rejected` and an `errors`
   array with the event's index and the reason. Accepted events in the same
   batch still land.

3. Query it back:

   ```bash
   trawl -p prod query 'service=ingest-check last=15m | head 10'
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
`Content-Encoding: gzip` is accepted. Each key gets
`[server.rate_limit] ingest_rpm` requests per minute on this route, 1000 by
default. For journald and file logs, follow
[Ship logs with Vector](/getting-started/vector-integration/).

## Receive syslog

1. Enable the listener in `/etc/trawl/trawld.toml`:

   ```toml
   [syslog]
   enabled = true
   udp_addr = "0.0.0.0:1514"
   tcp_addr = "0.0.0.0:1514"
   allow_cidrs = ["192.0.2.0/24"]
   default_service = "syslog"

   [syslog.source_service_map]
   "192.0.2.1" = "firewall"
   ```

   An empty `allow_cidrs` accepts every peer. UDP source addresses can be
   forged, and the listener has no TLS and no tokens, so limit who can reach
   the port.

2. Restart trawld. On Helm, set `config.syslog.enabled: true` and
   `service.syslog.enabled: true` to expose the ports.

3. Point one device at the host on port 1514, then query its service:

   ```bash
   trawl -p prod query 'service=firewall last=15m | head 10'
   ```

   Expect `_producer` = `syslog`, `host` from the frame, and the parsed
   `syslog_severity`, `syslog_timestamp`, and `syslog_facility` columns. `env`
   is `[ingest] default_env`. A source in `source_service_map` gets that
   service name whatever its APP-NAME says. An APP-NAME that fails the service
   charset falls back to `default_service` with the repair
   `service.from_profile`. The [`[syslog]` reference](/reference/configuration/#syslog)
   lists every key.

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
