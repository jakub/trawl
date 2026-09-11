---
title: Connect log producers
description: Verify HTTP and syslog ingestion and map collector fields deliberately.
---

HTTP, native syslog, and internal telemetry enter the same event canonicalizer.
The `_producer` field records which entry point actually accepted the event.
The [event reference](/reference/events/) owns field names, derivation, repairs,
and rejection rules. This page explains how to connect and verify a sender.

## HTTP ingestion

1. Select the direct trawld HTTPS endpoint. The browser proxy rejects `/api/v1/ingest`.
2. Create a service key whose roles grant `trawl:ingest`, using
   [access administration](/operate/access/#create-roles-and-keys).
3. Set a valid `service`, an allowed `env`, and the original `host`. Check the
   server's `ingest.envs` before sending a new environment.
4. Configure TLS trust. Use a protected curl config or the collector's credential
   mechanism for the bearer header; do not expose tokens in command output.
5. Send a bounded batch and inspect both accepted and rejected counts. Valid
   siblings can land even when another event is rejected.

With `TRAWL_URL` and `TRAWL_CURL_CONFIG` configured for the selected server:

```bash
curl --fail-with-body --config "$TRAWL_CURL_CONFIG" \
  -H 'Content-Type: application/json' \
  --data-binary '[{"service":"ingest-check","host":"collector-check","env":"prod","message":"connection check","level":"info"}]' \
  "$TRAWL_URL/api/v1/ingest"
trawl -p "$TRAWL_PROFILE" query 'service=ingest-check last=15m | head 10'
```

Replace `prod` with an allowed environment. Without a timestamp the event uses
arrival time and carries a time repair. Inspect `_producer`, `_severity`, and
`_repairs`, then use the same field mapping in the collector. This writes a real
event retained under the server's policy. The [API reference](/reference/api/#ingest)
defines accepted encodings and response behavior.

## Vector

Use the [Vector shipping guide](/getting-started/vector-integration/) for the sink,
remap, disk buffer, and shipped service configurations. Keep native event time,
service identity, and original host before batching. A trusted relay must provide
the original HTTP event host; otherwise that event is rejected rather than labeled
with the relay's address.

## Native syslog

Enable the listener and restrict its peers before exposing ports:

```toml
[syslog]
enabled = true
udp_addr = "0.0.0.0:1514"
tcp_addr = "0.0.0.0:1514"
allow_cidrs = ["192.168.10.0/24"]
default_service = "syslog"

[syslog.source_service_map]
"192.168.10.1" = "firewall"
```

Use your own subnet and mapping. Check `[ingest] default_env` and the allowlist;
the listener uses that environment. Restart the daemon to load the configuration.
For Helm, configure both the listener values and `service.syslog.enabled` to expose
its ports. Syslog transports do not use bearer tokens, and the native listener is
not a TLS syslog listener. Apply network access controls appropriate to the sender.

Send one known frame from the selected device and query its service and time.
Verify `_producer=syslog`, host, and the parsed `syslog_*` fields. The fixed profile
interprets numeric `syslog_severity` as syslog 0-7. A hostless frame from a trusted
relay is retained with host absent, unlike the HTTP rejection. An invalid APP-NAME
uses the configured fallback service with a repair. Full knobs are in
[syslog configuration](/reference/configuration/#syslog).

## Syslog over HTTP

An HTTP collector may forward the raw syslog severity numeral without converting
it. Declare that numeric source explicitly in trawld:

```toml
[ingest]
severity_from = [{ field = "syslog_severity", dialect = "syslog" }, "level"]
```

This derives `_severity` from `syslog_severity` and preserves the sender's field.
The first mappable source wins. Numeric OTel 1-24 remains the default for sources
without a dialect declaration; numeric values alone cannot identify the dialect.
`_producer` remains `http`, because the collector used HTTP. Configuration changes
affect new events and do not rewrite historical envelope values.

## Confirm continued delivery

Check collector retries and rejected events as well as successful requests.
Compare a known sender timestamp and event marker against a bounded query. Repairs
appear in `_repairs`; unmappable severity has its own counter and is not a repair.
If values become NULL after conformance, inspect [catalog conflicts](/operate/catalog/).
A queue or disk buffer is finite. Monitor its size and test outage behavior before
relying on it for a maintenance window.
