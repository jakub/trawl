---
title: Event contract
description: The declared envelope, derivation rules, repair codes, and rejection reasons for every ingested event.
---

Every accepted event passes through one canonicalizer. HTTP ingest, the syslog
listener, and internal telemetry select different producer profiles and share
every rule below.

## Declared fields

The envelope has ten declared fields. A declared field is not a value present
on every event: a missing column reads as NULL when a query combines
heterogeneous records.

| Field | Stored type | Set by | Description |
|-------|-------------|--------|-------------|
| `_time` | TIMESTAMP | derivation | Event instant. Derived from the configured sources, otherwise arrival time |
| `_ingested` | TIMESTAMP | trawld | Arrival instant. A sender cannot set it |
| `_raw` | VARCHAR | trawld, or the sender's proposal | The most original representation available, subject to the character cap |
| `_repairs` | VARCHAR | trawld | Comma-separated repair codes. Omitted for a clean event |
| `_severity` | SEVERITY over BIGINT | derivation | OpenTelemetry severity number, 1 to 24. Omitted when no source maps |
| `_producer` | VARCHAR | trawld | The door the event entered: `http`, `syslog`, or `trawld` |
| `env` | VARCHAR | sender or profile | Environment, validated against the configured allowlist |
| `service` | VARCHAR | sender or profile | Service name. An HTTP event without a usable name is rejected |
| `host` | VARCHAR | sender or profile | Origin host |
| `message` | VARCHAR | sender or profile | The sender's message, when it supplied one |

`env`, `service`, `host`, and `message` are the sender-asserted slots. A sender
names them, and a profile that can prove its own identity overrides a
conflicting payload value. HTTP can fill `host` from an honest peer address,
and the syslog and telemetry profiles can omit `host` when no honest identity
exists. The canonicalizer never invents a `message`.

The underscore namespace belongs to trawl. A sender field such as `timestamp`,
`severity`, or `level` stays an ordinary field after derivation. None of them
are query aliases.

## Names and original values

| Rule | Behavior |
|------|----------|
| Case | A field name folds to ASCII lowercase (`field.name_case_folded`) |
| Case collision | The exact lowercase spelling wins, otherwise the ASCII-lexicographically first variant. The loser is dropped (`field.name_case_collision`) |
| Name length | A name longer than 255 bytes drops its column (`field.name_too_long`) |
| Nested values | An object or array becomes JSON text |
| Reserved input name | The leading underscore run is stripped and the value lands under the bare remainder: `_HOSTNAME` becomes `hostname`, `__name__` becomes `name__` (`field.reserved_prefix`) |
| All-underscore name | Dropped (`field.reserved_prefix`) |
| Stripped name already present | The stripped loser is dropped (`field.reserved_prefix_collision`) |
| `_raw` cap | 65536 Unicode characters. Truncation records `field.truncated` |

A dropped field's name and value stay findable in `_raw`, up to the cap. Past
that boundary they are not recoverable.

The reserved-name rule has two exceptions: the `_time` proposal that time
derivation reads, and the `_raw` proposal the HTTP and syslog profiles may make.
A proposed string `_raw` is kept verbatim to the cap. Otherwise trawld
serializes the original object before repair and case folding, which is what
telemetry always gets. A sender cannot forge any other slot: an input
`_producer` or `_severity` lands as a bare `producer` or `severity`, and trawld
writes the real one.

| Name | Charset | Bounds |
|------|---------|--------|
| `env` | `[a-z0-9_-]` | 1 to 32 characters. `wal` and `scheduled` are reserved |
| `service` | ASCII letters, digits, `.`, `_`, `-` | 1 to 128 bytes, not dot-leading |

Path names are written verbatim after validation, so `api.v2` and `api_v2` stay
distinct.

## Producer profiles

| Producer | Identity it asserts | Fixed derivation sources |
|----------|---------------------|--------------------------|
| `http` | None. The sender owns `env`, `service`, and `host`, and the peer address is evidence for a fill | None |
| `syslog` | Configured env. Service from a source-IP mapping, a usable APP-NAME, or `default_service`. Host from the frame, or the peer address | `syslog_severity` in the syslog dialect, then `syslog_timestamp` |
| `trawld` | Configured env, service `trawld`, and the resolved hostname when one is available | None |

An asserted value beats a conflicting payload value. The displaced value stays
in `_raw`, and `field.producer_asserted` records the conflict. An equal value
and a JSON null are not conflicting assertions.

trawld rejects a hostname-less HTTP event behind a configured trusted relay
rather than attributing it to the relay. Syslog and telemetry cannot ask a
sender to resend, so they keep the event with `host` omitted and `host.omitted`.
trawld counts an unexpected profile rejection in
`trawl_ingest_profile_reject_total`.

## Time derivation

The configured default source order is `_time`, `timestamp`, `@timestamp`. A
profile's fixed sources come first, and a duplicate configured source cannot
replace a fixed source.

| Rule | Behavior |
|------|----------|
| Selection | The first source present wins, not the first parseable one |
| No usable value | Arrival time, recorded as `time.from_ingest`. An invalid first value does not fall through to a later source |
| Consumption | Only the `_time` proposal is consumed. Every bare source field stays in the event |
| Normalization | RFC 3339 UTC at microsecond precision |
| Plausibility | A parseable time more than ten years before arrival, or more than one day after it, is kept and flagged `time.out_of_range` |

Accepted string forms:

| Form | Example |
|------|---------|
| RFC 3339 | `2026-02-15T06:30:00.123456Z` |
| ISO basic offset | `2026-02-15T06:30:00+0530`, `2026-02-15T06:30:00+02` |
| Offset-less date-time, read as UTC | `2026-02-15 06:30:00`, `2026/02/15 06:30` |
| Date only, read as midnight UTC | `2026-02-15`, `2026/02/15` |

A `T` or a space separates the date from the time, and seconds and a fraction
are optional. A number is not an event-time encoding, and neither is a nested
value.

For an RFC 3164 frame with no year, the syslog listener considers the previous,
current, and next year in its local timezone, then takes the valid date nearest
arrival, breaking an exact tie toward the past. With no valid candidate,
ordinary derivation handles the missing time.

## Severity derivation

The configured default source order is `severity`, `severity_text`, `level`.

| Rule | Behavior |
|------|----------|
| Selection | The first source that maps to a severity wins |
| Unmappable source | Derivation falls through to the next source |
| No source maps | `_severity` is absent. That alone is not a repair |
| Numerals | Read on the OpenTelemetry ladder, 1 to 24, unless the source declares `dialect = "syslog"`, which reads 0 to 7 inverted |
| Words | Always read through the one token vocabulary, whichever dialect the source declares |
| Sources | Every source stays in the event under its own name |

A numeric range alone cannot establish a sender's dialect, so an HTTP source
known to carry a raw PRI numeral needs `dialect = "syslog"` written on it. The
syslog profile prepends that dialect for `syslog_severity`, so the raw numeral
stays stored while `_severity` takes the mapped value. The word vocabulary is in
the [DSL reference](/reference/dsl/).

A change to `[ingest] severity_from` or `[ingest] time_from` applies to events
that arrive after the configuration loads, and reinterprets no stored record.
Query-time `sev()` and a field repin are separate operations.

## Conformance outcomes

These four words name what happened to the data. They are not interchangeable.

| Outcome | Scope | What survives |
|---------|-------|---------------|
| Reject | One whole event, at ingest | Nothing lands. The response carries a typed reason |
| Repair | One event, at ingest | The event lands, with a code in `_repairs` |
| Drop | One field, at ingest | The rest of the event lands. The field's name and value stay in `_raw` |
| Shelve | One value, at conform time | The typed column reads NULL, `_raw` keeps the value, the shelving counts as a conflict, and a repin can resurrect it |

A shelved value is a catalog event, not an ingest repair, and it adds no repair
code. See [catalog conformance](/architecture/catalog/).

## Repair codes

`_repairs` carries each applicable code once, separated by commas. Deriving a
value is not a repair. Repair metrics use the same vocabulary.

| Code | What happened |
|------|---------------|
| `host.from_peer` | A missing host was filled from an honest peer address |
| `host.omitted` | A profile kept the event with no honest host value |
| `env.defaulted` | A missing environment used `[ingest] default_env` |
| `service.from_profile` | A profile used its service default after an unusable frame service |
| `time.from_ingest` | Event time was missing or invalid, so arrival time was used |
| `time.out_of_range` | A parseable event time was kept outside the plausibility window |
| `field.truncated` | `_raw` exceeded its character cap |
| `field.reserved_prefix` | A reserved prefix was stripped, or an all-underscore name was dropped |
| `field.reserved_prefix_collision` | A stripped name collided with an existing bare name |
| `field.name_too_long` | A field name exceeded 255 bytes and its column was dropped |
| `field.name_case_folded` | A field name changed to ASCII lowercase |
| `field.name_case_collision` | Two names differed only in ASCII case, and the losing one was dropped |
| `field.producer_asserted` | A profile's asserted identity displaced a conflicting payload value |

## Rejection reasons

trawld rejects an event rather than guessing an environment or a service. Each
reason is also its metric label value.

| Reason | Cause |
|--------|-------|
| `missing_service` | No `service` key |
| `service_not_string` | `service` present but not a JSON string |
| `empty_service` | `service` is an empty string |
| `service_too_long` | `service` exceeds 128 bytes |
| `invalid_chars` | `service` fails the service charset, or starts with a dot |
| `invalid_env` | `env` is not a string, or fails the env charset |
| `env_not_allowed` | `env` is valid in shape but is not in the effective env allowlist |
| `host_missing_from_relay` | `host` is missing and the peer is a configured trusted relay |
| `not_object` | The payload is not a JSON object |
| `invalid_json` | The body is not valid JSON |
| `wal_failure` | The write-ahead log write failed |
| `hot_buffer_full` | The hot buffer had no room for the request, which answered 503. See [hot-buffer admission](/architecture/data-flow/#hot-buffer-admission) |
| `ingest_batch_too_large` | The request was larger than external producers may place in the hot buffer, and answered 413 |

`hot_buffer_full` and `ingest_batch_too_large` reject a whole request, not one
event. They count every valid event in that request. Per-event rejections in
the same request keep their own reasons.

One batch can hold rejected events beside accepted siblings. The
[ingest response](/reference/api/#ingest) carries the counts and the errors.

## Syslog listener fields

The listener publishes what it parsed as ordinary columns, and the syslog
profile's fixed derivation sources read the first two.

| Column | Value | Present when |
|--------|-------|--------------|
| `syslog_severity` | The raw PRI severity numeral, 0 to 7 | The frame carried a PRI |
| `syslog_timestamp` | The frame time, as RFC 3339 UTC at microsecond precision | The frame carried a timestamp |
| `syslog_facility` | The facility name, such as `local0` | The facility is known |
| `syslog_pid` | The frame's PROCID | The frame carried one |
| `syslog_msgid` | The frame's MSGID | The frame carried one |
| `syslog_source_ip` | The peer address | Always |
| `sd_<id>_<param>` | One RFC 5424 structured-data parameter | The frame carried structured data |

Structured data is bounded at 32 elements and 128 parameters in total. Keys
arrive as the frame spelled them, then take the ordinary name rules. `_raw` is
the pre-parse wire line, and `env` comes from `[ingest] default_env`. An
APP-NAME that fails the service charset lands under `[syslog] default_service`
with a `service.from_profile` repair.
