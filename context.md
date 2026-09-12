# trawl

Shared vocabulary for the trawl log platform. This file is a glossary and nothing else — decisions live in `docs/adr/`, implementation detail in `CLAUDE.md` and the docs site.

## Language

### Actors

**Sender**:
Whatever emits events into ingest: vector, a syslog device, trawld observing itself. The sender owns bare-name vocabulary and the values of the sender-asserted slots.
_Avoid_: user, client, producer (that names the entry point, not the emitter)

**Operator**:
The human administering the install — the only actor who can repin, force a lossy change, or grant permissions.
_Avoid_: admin, user

**Client**:
Software consuming the API with a key: the CLI, TUI, web UI, or trawl-client. Rate limits and permissions attach to the client's key.
_Avoid_: user, consumer

### Ingest

**Envelope**:
The ten declared event fields: six trawl-owned `_` slots and four sender-asserted slots, `env`, `service`, `host`, and `message`. A declared field is not necessarily present on every event; its presence depends on the producer and available values.
_Avoid_: metadata, system fields, keywords

**Reserved name**:
Any field name starting with `_`. Reserved is a namespace rule, not a list: only trawl creates these names, and it is never said of `service` or the other sender-asserted slots.
_Avoid_: internal field, special field

**Producer**:
The entry point an event arrived through: http, syslog, or trawld (self-telemetry). Stamped on every event as `_producer`; senders cannot forge it.
_Avoid_: source, door, listener

**Profile**:
The fixed canonicalization behavior of one producer: what identity it asserts and which sources it reads first. One profile per producer.
_Avoid_: mode, pipeline config

**Canonicalization**:
The single step every event passes through at ingest, regardless of producer. It captures `_raw`, enforces the namespace rules, and fills the envelope.
_Avoid_: normalization, sanitization, the door

**Visible**:
An event is visible when a query can return it — it is in the hot buffer or in a finished parquet file. An event that is only in the WAL is safe on disk but not visible yet.
_Avoid_: queryable, searchable, live

### Storage

**Hot buffer**:
The in-memory store of freshly ingested events, visible to every query until compaction drains it. "Hot" always means this buffer's contents.
_Avoid_: cache, staging area

**Hot snapshot**:
The hot buffer's atomic view handed to one query: one file, plus exactly the pins that apply to the fields in it.
_Avoid_: snapshot (unqualified)

**Cold data**:
Finished parquet files, hourly or daily. Cold never includes the WAL.
_Avoid_: archive, on-disk data (the WAL is on disk too)

**WAL**:
The durability log between ingest and compaction. An event recorded in the WAL may also be visible through the hot buffer; WAL storage alone does not make it visible.
_Avoid_: journal, buffer

**Marker**:
A small file at the top of the data directory stating a fact about the whole archive: its epoch, which catalog owns it, a repin in progress. After a crash the marker is also the authority — it licenses recovery actions, like deleting a staging root.
_Avoid_: lockfile, flag file

**Epoch**:
A generation of the stored data's format and meaning. Anything that would make existing files lie under current rules bumps it; boot sets an old-epoch root aside instead of migrating it.
_Avoid_: schema version, migration level

**Compaction**:
The step that turns WAL batches into hourly parquet files — the moment events become cold. Never used for the hourly-to-daily merge.
_Avoid_: flush, rollup

**Rollup**:
The merge of one day's hourly parquet files into a single daily file. It is lossless file consolidation — not downsampling, despite what the word means in metrics systems.
_Avoid_: compaction, downsampling

### Catalog

**Pin**:
The type the catalog has recorded for a field name. Once a field is pinned, all of trawl uses that type: data written to disk is cast to it, and query comparisons follow its rules.
_Avoid_: inferred type, schema type, type hint

**Contract-typed**:
A pin that belongs to the envelope. An operator can retype (repin) an ordinary field, but never one of these.
_Avoid_: system-typed, locked, built-in

**Observation**:
A record that a (field, service) pair was seen at least once, and when it was last seen. It claims nothing about whether that data still exists; readers must window on `last_seen` themselves.
_Avoid_: presence, liveness

**Degraded pin**:
A pin the analyzer judges to be doing sustained damage: its type keeps shelving real values. The pin is what is wrong and a repin is the fix — say "degraded pin", not "degraded field" (the wire key `degraded_fields` is accepted shorthand).
_Avoid_: degraded field (outside the wire), broken field

**Conflict**:
The record that conform shelved rows for a (field, service) pair: a tally, sample values, and aggregates the degraded-pin analyzer reads.
_Avoid_: type error, schema mismatch

**Repin**:
An operator-triggered change to one field's pinned type across the whole stored corpus. Its effect on query results follows the comparison contract; a repin does not promise unchanged results for every query.
_Avoid_: migration, retype (as a noun)

**Resurrection**:
The repin rewrite's recovery of shelved values: re-extracting the original from `_raw` under the same lossless guard, so data a bad pin shelved returns under the new type.
_Avoid_: backfill, recovery (too generic)

### Query

**Pin snapshot**:
The full catalog pin set, taken once and used consistently for one query — or for an SSE stream's whole life, which is why a repin only reaches a running stream at reconnect.
_Avoid_: snapshot (unqualified)

**Lane**:
One of the four evaluators that can answer a query: batch SQL, the live stream (SSE), the kv batch tail, and embedded `--data`. "All four lanes" means literally all four; when embedded is excluded (it has no catalog), say "the three pinned lanes".
_Avoid_: path, mode

**Conform**:
To cast a value to its pin under the lossless round-trip guard — one operation, wherever it runs (compaction, a query's hot branch, the boot pass, a repin rewrite). A cast that would change the value shelves it instead.
_Avoid_: coerce, normalize

**Field**:
The logical name and value: what a sender wrote or trawl guarantees, what the DSL references, what the catalog pins.
_Avoid_: column (that is its storage form), key, attribute

**Column**:
A field's physical form in a parquet file or a DuckDB result. You pin a field; you read a column.
_Avoid_: field (when talking about storage or SQL output)

**Bind**:
DuckDB's prepare/plan phase, before any row is read. A bind can keep an executor permit occupied after the request ends because cancellation does not reliably stop this phase.
_Avoid_: prepare, compile, plan (as a verb)

**Lateral expansion**:
Additional expression work caused by substituting earlier same-stage targets, including repeated references introduced by SQL translation. Independent expressions and flat lists without such references have zero lateral expansion.
_Avoid_: query complexity, bind cost, node count

**Retained permit**:
An executor-pool permit still held by physical work after its request has ended. Request completion and permit reclamation are separate events.
_Avoid_: leaked permit, stuck query (the permit is the thing named)

### Schedules

**Schedule**:
The cadence, window and lag attached to one saved query. Replacing a schedule replaces the whole shape: a schedule without a window is in query mode, and "omitted" never means "unchanged".
_Avoid_: cron, timer, job (that is a repin)

**Window**:
What one scheduled run covers in event time: absent (the query text owns its time), since the previous run's covered point, or a fixed trailing span ending at the run. Half-open.
_Avoid_: range (that is the search page's control), period, report window (the wire name is accepted shorthand)

**Lag**:
The late-arrival allowance that moves both bounds of a window back. It exists only beside a window; zero is spelled as absence.
_Avoid_: delay, grace period, offset

### Outcome verbs

Ordered by blast radius. These are the words for "what happened to the data", so precision here is an incident-response concern.

**Reject**:
Refuse a whole event at ingest, with a typed reason. Nothing lands.
_Avoid_: drop, discard

**Repair**:
Alter an event at ingest where the server has an honest answer, recording a code in `_repairs`. The event lands, with a scar.
_Avoid_: fix up, coerce

**Drop**:
Discard one field at ingest; the rest of the event lands, and the field's name and value stay findable in `_raw`.
_Avoid_: reject, strip

**Shelve**:
Null a value at conform time because the cast would have changed it. The value survives in `_raw`, the shelving is counted as a conflict, and a repin can resurrect it.
_Avoid_: drop, lose, null out
