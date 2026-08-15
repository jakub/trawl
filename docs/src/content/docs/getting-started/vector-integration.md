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
| `_ingested` | TIMESTAMP | trawl | arrival time; a client value takes the reserved-prefix strip and lands as a plain `ingested` column (`field.reserved_prefix`) |
| `_raw` | VARCHAR | you or trawl | most original form available: send a string `_raw` to preserve your pre-parse line, else trawl stores the pre-repair serialization of what arrived. Bare-word search reads this column, so under the serialization a bare term matches any field's value **and** any field name — see [Text search](/reference/dsl/#text-search) |
| `_repairs` | VARCHAR | trawl | comma-separated repair codes; NULL for untouched events |
| `env` | VARCHAR | you (or `default_env`) | path dimension; must be in the server's `ingest.envs` allowlist or the event **rejects** |
| `service` | VARCHAR | you — **required** | shard key and filename; missing or non-string values **reject**. Charset `[A-Za-z0-9._-]`, no spaces, no leading dot, ≤128 bytes |
| `host` | VARCHAR | you (or peer IP) | filled from the sender address when absent (`host.from_peer`) — **rejected instead** when the peer is a configured trusted relay |
| `_severity` | SEVERITY | trawl | derived OTel SeverityNumber 1-24; **derivation-only** — a client-sent `_severity` strips to a plain `severity` column, which the derivation then reads |
| `message` | VARCHAR | you, by convention | the important part of the line |

Everything else you send is **your** vocabulary, stored verbatim under
the name you sent it. The whole `_` prefix is trawl's, though: a
non-proposable `_x` has its leading underscores stripped and lands under
the bare remainder (`_HOSTNAME` → `hostname`, `__name__` → `name__`,
`field.reserved_prefix`), so a journald or prometheus passthrough keeps
every field queryable instead of losing it.

### Derivation sources: read, never consumed

trawl READS these keys to fill its own slots and **stores every one of
them verbatim** as your own column:

- **`_time`** ← `_time`, then `timestamp`, then `@timestamp`; first
  PRESENT wins. Accepted grammar: RFC 3339, ISO 8601 basic offsets
  (`+0530`, `+02`), offset-less date-times (read as UTC), `T` or space
  separator, `YYYY-MM-DD` or `YYYY/MM/DD`, optional seconds/fraction, or
  a bare date (midnight UTC). Only `_time` itself is consumed — it is
  the proposal slot — so `timestamp` and `@timestamp` stay queryable
  beside the canonical instant.
- **`_severity`** ← `severity`, then `severity_text`, then `level`;
  first MAPPABLE wins. All three stay as ordinary columns whatever the
  derivation decides, so `{"service":"game","level":"gold"}` keeps a
  fully queryable `level="gold"` and simply gets no `_severity`.

So an existing remap that sets `.timestamp` and `.level` keeps working —
and now keeps its fields too. The shipped configs emit `._time`
natively, which is preferred.

### Severity tokens

`severity`, `severity_text` and `level` values are matched
case-insensitively against this table (plus OTel's exact short names —
`trace2`, `warn3`, `error2`, …). Anything unmappable simply leaves
`_severity` absent — no rejection, and **no repair code**, because
nothing you sent was touched. The ops signal is the
`trawl_severity_unmapped_total{service}` counter:

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

A **numeric** source — JSON number or numeric string — is read
**strictly as OTel 1-24**, never syslog-inverted: `3` is `trace`, and
`0` or `25` map to nothing. The two ranges overlap, so no value-shape
rule could tell the dialects apart; syslog inversion happens only in
trawl's own syslog listener, where the transport proves the dialect.
A collector forwarding syslog over HTTP should remap at the collector.

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
._time = .timestamp            # or leave `timestamp` — it is read as a source
.env = "${TRAWL_ENV:-prod}"    # must be in the server's ingest.envs
.service = "myapp"             # required — events without it are rejected
.host = get_hostname!()
.severity = "info"             # trawl derives `_severity` and keeps this
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

## Generate adversarial test logs

The repository task runner emits deterministic newline-delimited JSON that is
valid input for both Vector's JSON file source and trawl's HTTP ingest route:

```bash
cargo xtask ingest-fuzz --phase mutate --seed 42 --events 1000 > /tmp/trawl-fuzz.ndjson
curl -fsS \
  -H "Authorization: Bearer $TRAWL_INGEST_TOKEN" \
  -H 'Content-Type: application/x-ndjson' \
  --data-binary @/tmp/trawl-fuzz.ndjson \
  "$TRAWL_URL/api/v1/ingest"
```

The generator writes only NDJSON to stdout; its row-count summary goes to
stderr. `--seed`, `--namespace`, `--env`, and `--events` make it straightforward
to wrap in a shell loop or CI job. The `mutate` phase emits accepted events
covering scalar boundaries, nested values, timestamp and severity aliases,
case-colliding and path-shaped field names, and the server's repair paths. The
`rejects` phase emits valid JSON objects with invalid envelope fields so a test
can assert the per-event rejection response.

Schema-pinning runs are deliberately split into phases. Post and compact the
`pin` file before posting `conflicts`; otherwise both sets may share the first
DuckDB inference batch and test a different contract:

```bash
cargo xtask ingest-fuzz --phase pin --seed 42 --namespace ci > /tmp/pin.ndjson
# POST /tmp/pin.ndjson, then wait for its WAL batch to compact.
cargo xtask ingest-fuzz --phase conflicts --seed 42 --namespace ci > /tmp/conflicts.ndjson
# POST /tmp/conflicts.ndjson, compact, then inspect schema/conflict results.
```

The pin phase installs direct BIGINT, DOUBLE, TIMESTAMP, BOOLEAN, and VARCHAR
candidates, an all-null deferred candidate, and mixed candidates at exactly
9/10 and 8/10 successful BIGINT readings. Reusing the seed and namespace is
load-bearing: they derive the shared field names. Use a disposable trawl data
root and catalog for these runs; generated fields intentionally spend catalog
pin slots.

For a Vector file source, decode each line as JSON before connecting it to the
normal HTTP sink:

```toml
[sources.trawl_fuzz]
type = "file"
include = ["/tmp/trawl-fuzz.ndjson"]
read_from = "beginning"
framing.method = "newline_delimited"
decoding.codec = "json"
```

## What the server records

Every server-side substitution is visible: the event's `_repairs` column
carries codes (`host.from_peer`, `env.defaulted`, `time.from_ingest`,
`time.out_of_range`, `severity.unmapped`, `field.truncated`,
`meta.stripped`, `field.name_too_long`, `field.name_case_folded`,
`field.name_case_collision`) and
`trawl_ingest_repairs_total{code, service}` counts
them on `/metrics`. `stats count() by _repairs, service` shows which
senders the server is having to patch.
