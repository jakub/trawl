---
title: Event contract
description: Declared fields, producer profiles, derivation, omissions, and repair codes.
---

All accepted events pass through one canonicalizer. HTTP ingest, syslog, and internal telemetry select different producer profiles, but share field-name rules, nested-value handling, raw-value limits, and repair reporting.

## Declared fields

There are ten declared fields, not ten values guaranteed to be present on every event. Missing columns read as NULL when a query combines heterogeneous records.

| Field | Meaning and presence |
| --- | --- |
| `_time` | Required event instant. Derived from configured sources, otherwise arrival time. Stored as a UTC timestamp after compaction. |
| `_ingested` | Required server-stamped arrival instant. A sender cannot set it. |
| `_raw` | Required original representation, subject to the character cap below. |
| `_repairs` | Comma-separated repair codes. Omitted for a clean event. |
| `_severity` | Derived OpenTelemetry severity number, 1-24. Omitted when no source maps to a severity. |
| `_producer` | Required server-stamped producer: `http`, `syslog`, or `trawld`. |
| `env` | Required environment, validated against the configured allowlist. |
| `service` | Required service name. HTTP events without a usable name are rejected. |
| `host` | Sender or profile identity. HTTP can fill an honest peer address; syslog or telemetry can omit it when no honest identity exists. |
| `message` | Sender's message, when supplied. The canonicalizer does not require or invent one for every HTTP event. |

The underscore namespace belongs to Trawl. Bare names belong to the sender, except that `env`, `service`, `host`, and `message` have declared roles in the envelope. Sender fields such as `timestamp`, `@timestamp`, `severity`, `severity_text`, and `level` remain ordinary fields after derivation. They are not query aliases.

## Names and original values

Field names fold to ASCII lowercase before downstream processing. If variants collide, the exact lowercase spelling wins; otherwise the ASCII-lexicographically first variant wins. Field names longer than 255 bytes are dropped as columns. Nested objects and arrays become JSON text; the original representation remains in `_raw` subject to its cap.

A reserved input name loses its leading underscore run and becomes a bare name: `_HOSTNAME` becomes `hostname`, and `__name__` becomes `name__`. A name made only of underscores is dropped. If the bare name already exists, it wins. Two exceptions are the `_time` proposal used by time derivation and the `_raw` proposal on HTTP and syslog profiles. An input `_producer` becomes bare `producer`; Trawl then stamps the real `_producer`.

HTTP and syslog may propose a string `_raw`, which is kept verbatim up to 65,536 characters. Otherwise Trawl serializes the original object before repair and case folding. Telemetry uses that serialization even if its payload supplied `_raw`. The cap counts Unicode characters, not bytes, and truncation records `field.truncated`. Once raw text is truncated, values beyond that boundary cannot be recovered from it.

Environment names use `[a-z0-9_-]`, 1-32 characters, excluding reserved directory names such as `wal` and `scheduled`. Service names use ASCII letters, digits, dot, underscore, and hyphen; they must not start with a dot and are limited to 128 bytes. Path names are written verbatim after validation, so `api.v2` and `api_v2` remain distinct.

## Producer profiles

| Producer | Identity | Fixed derivation sources |
| --- | --- | --- |
| `http` | Sender service; sender env or configured default; sender host or honest peer address | None |
| `syslog` | Configured env; source-IP mapping, usable APP-NAME, or configured service default; frame hostname or honest peer address | `syslog_severity` in syslog dialect; `syslog_timestamp` |
| `trawld` | Configured env, service `trawld`, resolved hostname when available | None |

A profile's asserted identity takes precedence over a conflicting payload value. The displaced value remains in `_raw`, and `field.producer_asserted` records the conflict. Equal values and JSON null do not count as conflicting assertions.

A hostname-less HTTP event behind a configured trusted relay is rejected rather than attributed to the relay. Syslog and telemetry cannot ask the sender to retry, so they can keep an event with host omitted and `host.omitted`. An unusable syslog APP-NAME can use the configured default and record `service.from_profile`. Profile environments are validated at boot; an unexpected profile rejection is counted in `trawl_ingest_profile_reject_total`.

## Time derivation

The default source order is `_time`, `timestamp`, `@timestamp`. A profile's fixed sources precede the configured sources, and a duplicate configured source cannot replace a fixed source.

Time uses the first present source, not the first parseable one. An invalid first value, including null, falls back to arrival time rather than trying a later field. Only the `_time` proposal is consumed. Bare source fields stay in the event.

Accepted strings include RFC 3339, ISO basic offsets such as `+0530` and `+02`, offset-less date-times interpreted as UTC, and bare dates interpreted as midnight UTC. The parser accepts `T` or a space between date and time, `YYYY-MM-DD` or `YYYY/MM/DD`, and hours/minutes with optional seconds and fraction. Values normalize to RFC 3339 UTC with microsecond precision. Numbers and nested values are not event-time encodings.

An absent or invalid value records `time.from_ingest`. A parseable time more than ten years before arrival or more than one day after arrival is retained with `time.out_of_range`. This flag reports clock skew without silently changing a parseable instant.

The syslog listener stores its parsed frame time in `syslog_timestamp`. For RFC 3164 dates without a year, it considers the previous, current, and next year in the listener's local timezone, then chooses the valid date nearest arrival, breaking an exact tie toward the past. If no candidate is valid, ordinary derivation handles the missing time.

## Severity derivation

The default source order is `severity`, `severity_text`, `level`. Severity uses the first mappable source. An absent or unmappable source can fall through to the next one; if none maps, `_severity` is absent and that alone is not a repair.

The default numeric interpretation is the OpenTelemetry ladder, 1-24. Text tokens and exact names use the vocabulary in the [severity reference](/reference/dsl/#severity-_severity). Numeric ranges alone cannot establish a sender's dialect. A configured source can explicitly use `dialect = "syslog"`, including an HTTP source known to carry raw PRI severity 0-7. The syslog profile prepends that fixed dialect for `syslog_severity`, so its raw numeral remains stored while `_severity` receives the mapped value.

Changing derivation configuration affects future events after the configuration is loaded. It does not reinterpret existing records. Query-time `sev()` and a field repin are separate operations with their own contracts.

## Repairs

`_repairs` contains each applicable code once, separated by commas. Deriving a value is not itself a repair. Repair metrics use the same code vocabulary.

| Code | What happened |
| --- | --- |
| `host.from_peer` | A missing host was filled from an honest peer address. |
| `env.defaulted` | A missing environment used the configured default. |
| `time.from_ingest` | Event time was missing or invalid, so arrival time was used. |
| `time.out_of_range` | A parseable event time was retained outside the plausibility window. |
| `field.truncated` | `_raw` exceeded its character cap. |
| `field.reserved_prefix` | A reserved prefix was stripped, or an all-underscore name dropped. |
| `field.reserved_prefix_collision` | A stripped name collided with an existing bare name. |
| `field.name_too_long` | A field name exceeded the byte limit and its column was dropped. |
| `field.name_case_folded` | A field name changed to ASCII lowercase. |
| `field.name_case_collision` | Case variants described one column, and the losing variant was dropped. |
| `field.producer_asserted` | A profile's asserted identity displaced a conflicting payload value. |
| `host.omitted` | A profile kept the event without an honest host value. |
| `service.from_profile` | A profile used its service default after an unusable frame service. |

## Rejection and storage conflicts

HTTP rejects invalid event identity rather than guessing an environment or service. A batch can contain rejected events alongside accepted siblings; consult the [ingest API response](/reference/api/) for counts and errors. Invalid JSON and WAL failures are separate failures from repair.

A catalog conflict occurs later, when a value cannot conform to its pinned type. It writes NULL in the typed column and records conflict evidence. This is not an ingest repair and does not add a repair code. See [catalog conformance](/architecture/catalog/) and the [schema procedure](/reference/cli/).

## Contract owners

The implementation is [envelope.rs](https://github.com/jakub/trawl/blob/main/crates/trawl-server/src/ingest/envelope.rs), [producer.rs](https://github.com/jakub/trawl/blob/main/crates/trawl-server/src/ingest/producer.rs), and [schema.rs](https://github.com/jakub/trawl/blob/main/crates/trawl-core/src/schema.rs). The governing decisions are [ADR-0009](/contribute/decisions/#adr-0009) and [ADR-0013](/contribute/decisions/#adr-0013).
