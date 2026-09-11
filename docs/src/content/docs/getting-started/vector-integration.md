---
title: Ship logs with Vector
description: Map sender fields, configure the HTTP sink, and verify accepted events.
---

Vector collects, transforms, buffers, and sends events to trawld's direct HTTPS
API. This guide assumes that the server is running and you have a service key
with `trawl:ingest`. Use [access administration](/operate/access/#create-roles-and-keys)
to create it. The browser proxy is not an ingest endpoint.

## The event schema

Use the [event reference](/reference/events/) for the complete envelope and
repair/rejection contract. In the remap, choose a valid service, preserve the
original host, and select an environment in the server's `ingest.envs` allowlist.
Fields are ASCII-folded at ingest; the `_` prefix belongs to Trawl. `_time` and
string `_raw` are supported proposal fields; `_severity` is derived by the server.

### Derivation sources: read, never consumed

`timestamp` and `@timestamp` remain queryable after time derivation. Ordinary
severity sources also survive. Only `_time` itself is consumed as the timestamp
proposal. See [derivation configuration](/reference/configuration/#derivation-sources-severity_from--time_from)
when the sender uses other names or numeric syslog severity.

### Severity tokens

The [event reference](/reference/events/) owns tokens and numeric interpretations.
OTel 1-24 is the default numeric dialect. HTTP collectors can explicitly declare
a syslog source; see [syslog over HTTP](/operate/ingestion/#syslog-over-http).
An unmappable value leaves `_severity` absent without a repair or rejection.

### env

Set the remap's environment to one allowed by the server. A missing environment
uses `default_env` with a repair; an unlisted environment is rejected. Changing a
collector's environment also changes where its events are stored, so verify a
sample before rolling the mapping out to every sender.

## Basic sink configuration

Combine this sink with the remap below and your existing source. Supply
`TRAWL_URL` and `TRAWL_INGEST_TOKEN` through Vector's protected service environment.
`TRAWL_URL` must name the direct trawld API, for example `https://logs.example.com:5514`.
The example uses a CA file that validates the server certificate; configure its
actual path. Do not routinely disable certificate verification.

```toml
[sinks.trawl]
type = "http"
inputs = ["trawl_schema"]
uri = "${TRAWL_URL}/api/v1/ingest"
encoding.codec = "json"
compression = "gzip"
batch.max_bytes = 1048576
batch.timeout_secs = 5

[sinks.trawl.request]
headers.authorization = "Bearer ${TRAWL_INGEST_TOKEN}"

[sinks.trawl.tls]
ca_file = "/etc/vector/trawl-ca.pem"
verify_certificate = true

[sinks.trawl.buffer]
type = "disk"
max_size = 1073741824
when_full = "block"
```

A disk buffer needs writable persistent Vector storage. Its capacity is finite;
`when_full = "block"` propagates backpressure instead of promising unlimited
outage coverage. Validate retry behavior for the installed Vector version,
particularly authentication and per-event ingest rejections.

## A minimal remap

Replace `your_source` with the name of the source in your complete Vector config.
Set `service` and `env` for that source. This remap assumes each event represents
the collector's own host; a relay should preserve the originating device instead.

```toml
[transforms.trawl_schema]
type = "remap"
inputs = ["your_source"]
source = '''
._time = .timestamp
.env = "prod"
.service = "myapp"
.host = get_hostname!()
# Preserve an existing level or severity field for server-side derivation.
# ._raw = .message  # retain a pre-parse line if available
'''
```

An absent timestamp will be repaired to arrival time. Do not assign every event a
constant severity just to fill the column: that can hide the sender's real level.
For a known syslog numeral, declare the server source dialect or normalize it once
at the collector, then verify both the raw and canonical values.

## Shipped configs

The repository contains `config/vector/local-dev.toml` for embedded-mode demo
files and `config/vector/debian/base.toml` with service drop-ins for Debian.
The embedded demo materializes fields itself because no server canonicalizer runs
in that path. It is not a replacement for the HTTP collector configuration.

The Debian base reads journald and selected `/var/log` files. Drop-ins cover nginx,
Apache, PostgreSQL, MySQL, Redis, Docker, fail2ban, and UniFi syslog. Select sources
and permissions for the actual host; do not install unrelated collector access
merely because a sample config lists it.

## Validate and verify delivery

1. Run the installed Vector version's config validation against the complete
   configuration and service environment. Check `vector validate --help` for its
   supported options. TOML parsing alone does not validate VRL or sink behavior.
2. Reload or restart only the selected collector, then inspect its diagnostics
   without exposing the token-bearing environment.
3. Generate one known event at the source and query its service with a short time
   window. Compare host, event time, `_producer=http`, and derived severity.
4. Inspect rejected events, repair counters, and buffer/retry state. An HTTP response
   can include rejected events alongside accepted siblings.

Use [ingestion checks](/operate/ingestion/) for a bounded direct POST that isolates
collector problems from server problems. Use [catalog diagnosis](/operate/catalog/)
when values arrive but conformance shelves them as NULL.

## Generate adversarial test logs

The deterministic generator and its producer profiles are contributor tools.
Use [adversarial ingest testing](/contribute/testing/#generate-adversarial-test-logs)
on disposable storage and catalog state. Generated field names consume pin slots.

## What the server records

The canonical [repair-code reference](/reference/events/) defines `_repairs` and
`trawl_ingest_repairs_total`. Query repairs by service to identify sender problems.
Unmappable severity uses `trawl_severity_unmapped_total`, not a repair code.
Original sender values remain available subject to the documented raw-size limit.
