---
title: Vector Integration
description: Configure Vector to ship logs to trawl — the field mapping is the schema.
---

[Vector](https://vector.dev) is the recommended log shipper for trawl. It
handles collection from multiple sources, batching, compression, and
reliable delivery. Your Vector remap **is** the schema in practice, so this
page is mostly about the field mapping.

## The event schema

trawl declares a ten-field envelope (ADR-0009). The namespace rule: a `_`
prefix marks metadata about the record's handling; no prefix means data
about the event.

| Field | Type | Who sets it | Notes |
|---|---|---|---|
| `_time` | TIMESTAMP | you (or trawl repairs) | event time; missing/unparseable values are replaced with the arrival time and flagged `time.from_ingest` |
| `_ingested` | TIMESTAMP | trawl | arrival time; client values are stripped (`meta.stripped`) |
| `_raw` | VARCHAR | you or trawl | most original form available: send a string `_raw` to preserve your pre-parse line, else trawl stores the pre-repair serialization of what arrived |
| `_repairs` | VARCHAR | trawl | comma-separated repair codes; NULL for untouched events |
| `env` | VARCHAR | you (or `default_env`) | path dimension; must be in the server's `ingest.envs` allowlist or the event **rejects** |
| `service` | VARCHAR | you — **required** | shard key and filename; missing or non-string values **reject**. Charset `[A-Za-z0-9._-]`, no spaces, no leading dot, ≤128 bytes |
| `host` | VARCHAR | you (or peer IP) | filled from the sender address when absent (`host.from_peer`) — **rejected instead** when the peer is a configured trusted relay |
| `severity` | INTEGER | derived | OTel SeverityNumber 1-24, derived server-side |
| `severity_text` | VARCHAR | you | your original severity spelling, kept verbatim |
| `message` | VARCHAR | you, by convention | the important part of the line |

### Accepted wire aliases

trawl consumes these keys at ingest — they feed the envelope and are never
stored as columns:

- **`timestamp`** or **`@timestamp`** → the `_time` input (precedence:
  `_time`, then `timestamp`, then `@timestamp`). Accepted grammar: RFC
  3339, ISO 8601 basic offsets (`+0530`, `+02`), offset-less date-times
  (read as UTC), `T` or space separator, `YYYY-MM-DD` or `YYYY/MM/DD`,
  optional seconds/fraction, or a bare date (midnight UTC).
- **`level`** → the severity-derivation input (the DSL's `level` alias
  would shadow a stored column anyway; the original is always in `_raw`).

So an existing remap that sets `.timestamp` and `.level` keeps working
unchanged — the shipped configs emit `._time` and `.severity_text`
natively, which is preferred.

### Severity tokens

`severity`, `severity_text` and `level` values are matched
case-insensitively against this table; anything else leaves `severity`
NULL with the `severity.unmapped` repair code (never a rejection) and is
preserved as `severity_text`:

| tokens | number |
|---|---|
| `trace`, `t` | 1 |
| `debug`, `d` | 5 |
| `info`, `i` | 9 |
| `notice` | 10 |
| `warn`, `warning`, `w` | 13 |
| `error`, `err`, `e` | 17 |
| `fatal`, `critical`, `crit`, `f` | 21 |
| `alert` | 23 |
| `emerg`, `panic` | 24 |

Syslog numerics 0-7 are accepted and **inverted** onto the OTel ladder
(syslog counts down from Emergency 0): 7→5, 6→9, 5→10, 4→13, 3→17, 2→21,
1→23, 0→24. A client-supplied integer `severity` in 1-24 always wins;
then a string `severity` (`{"severity":"ERROR"}`, the GCP/Stackdriver
shape); then `severity_text`; then `level`. An **integer** `severity` is
only ever read on the OTel ladder — never syslog-inverted, because `0` is
OTel's UNSPECIFIED as readily as it is syslog's Emergency — so an
out-of-ladder integer maps to nothing and is kept as `severity_text`
rather than guessed at.

### env

`env` is infrastructure topology and is **allowlisted in the server
config** (`[ingest] envs`, default `[default_env]`, default `prod`):

- an event without `env` lands under `default_env` (repair code
  `env.defaulted`)
- an event with an env **not in the allowlist is rejected** — repairing it
  would misfile data in the wrong path root permanently

Set `.env` in your remap when you run more than one env (the shipped
debian configs use `${TRAWL_ENV:-prod}`), and make sure the value is in
the server's allowlist.

## Basic sink configuration

```toml
[sinks.trawl]
type = "http"
inputs = ["your_remap"]
uri = "https://your-trawl-server:5514/api/v1/ingest"
encoding.codec = "json"
compression = "gzip"
batch.max_bytes = 1048576
batch.timeout_secs = 5

[sinks.trawl.request]
headers.authorization = "Bearer ${TRAWL_INGEST_TOKEN}"

[sinks.trawl.tls]
verify_certificate = false  # if using self-signed certs
```

## A minimal remap

```toml
[transforms.trawl_schema]
type = "remap"
inputs = ["your_source"]
source = '''
._time = .timestamp            # or leave `timestamp` — it is consumed as an alias
.env = "${TRAWL_ENV:-prod}"    # must be in the server's ingest.envs
.service = "myapp"             # required — events without it are rejected
.host = get_hostname!()
.severity_text = "info"        # trawl derives numeric severity
# ._raw = .message             # optionally preserve your pre-parse line
del(.source_type)
'''
```

## Shipped configs

The repo ships ready-made configs under `config/vector/`:

- `local-dev.toml` — demo sources writing ndjson for embedded-mode
  queries (materializes the full envelope including numeric `severity`,
  since no server canonicalizer is in the path)
- `debian/base.toml` — journald + /var/log catch-all with the HTTP sink
- `debian/{nginx,apache,postgresql,mysql,redis,docker,fail2ban,unifi-syslog}.toml`
  — per-service drop-ins, safe to deploy everywhere

## What the server records

Every server-side substitution is visible: the event's `_repairs` column
carries codes (`host.from_peer`, `env.defaulted`, `time.from_ingest`,
`time.out_of_range`, `severity.unmapped`, `field.truncated`,
`meta.stripped`) and `trawl_ingest_repairs_total{code, service}` counts
them on `/metrics`. `stats count() by _repairs, service` shows which
senders the server is having to patch.
