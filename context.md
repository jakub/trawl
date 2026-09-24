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

**Admission**:
A producer reserving hot-buffer space for a batch before writing it to the WAL. A batch that cannot be admitted is refused whole, and nothing is written. Nothing admitted leaves the hot buffer except through compaction.
_Avoid_: backpressure (that is the sender's experience of a refusal), eviction (removed)

**Hot snapshot**:
The hot buffer's atomic view handed to one query: one file, plus exactly the pins that apply to the fields in it.
_Avoid_: snapshot (unqualified)

**Cold data**:
Finished parquet files, hourly or daily. Cold never includes the WAL.
_Avoid_: archive, on-disk data (the WAL is on disk too)

**WAL**:
The durability log between ingest and compaction. An event recorded in the WAL may also be visible through the hot buffer; WAL storage alone does not make it visible.
_Avoid_: journal, buffer

**Quarantine**:
A WAL, Parquet or temporary rollup file removed from normal processing because it is corrupt or unreadable, with its bytes retained for investigation. Quarantine does not establish how many events are lost or whether other copies can recover them.
_Avoid_: deletion, event loss

**Marker**:
A small file at the top of the data directory stating a fact about the whole archive: its epoch, which catalog owns it, a repin in progress. After a crash the marker is also the authority — it licenses recovery actions, like deleting a staging root.
_Avoid_: lockfile, flag file

**Publication marker**:
The durable record of one compaction publishing its WAL batches into a parquet file: which WAL files it consumes and which output it installs. While it exists, recovery decides from it whether the publish happened, and nothing else may compact, hydrate, retire or delete the files it names.
_Avoid_: lock, journal entry

**Epoch**:
A generation of the stored data's format and meaning. Files in one epoch share the same interpretation rules.
_Avoid_: schema version, migration level

**Compaction**:
The step that turns WAL batches into hourly parquet files — the moment events become cold. Never used for the hourly-to-daily merge.
_Avoid_: flush, rollup

**Rollup**:
The merge of one day's hourly parquet files into a single daily file. It is lossless file consolidation — not downsampling, despite what the word means in metrics systems.
_Avoid_: compaction, downsampling

**Deletion floor**:
The free space trawl keeps on the data filesystem (`min_free_disk_bytes`). Falling below it starts pressure deletion; it reserves nothing and does not stop writes.
_Avoid_: reserve, quota, threshold (unqualified)

**Headroom**:
The free space on one filesystem trawl writes to, and on the data filesystem, how far it sits above or below the deletion floor. Headroom belongs to a filesystem, never to a directory or an environment.
_Avoid_: capacity (the Health card of that name is about uptime and pools), disk left

**Pressure deletion**:
Retention deleting date directories because free space fell below the deletion floor, ahead of their age limit. It shortens retention; the disk stays above the floor while it does.
_Avoid_: cleanup, eviction

**Observed day**:
One of an environment's recent, stored date partitions that a capacity projection reads for its daily volume. Today and yesterday are never observed days; they are still settling.
_Avoid_: settled day, sample

**Retention reach**:
How many days of an environment's configured retention the disk is projected to hold if its observed days repeat. A projection, not a guarantee, and never a countdown to a full disk.
_Avoid_: days left, days until full, forecast (unqualified)

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
The logical name and value of an event: what a sender wrote or trawl guarantees, what the DSL references, what the catalog pins. Only events have fields; a table has columns and an analysis declares output columns.
_Avoid_: column (that is its storage form), key, attribute, output field (an analysis declares output columns)

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

**Metered**:
Counted by the per-key rate limiter. A metered request is attributable to a verified key and bounded by that key's rate; an unmetered one reached the server before any key was counted.
_Avoid_: authenticated (a request can be authenticated and still refused before metering)

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

**Manual run**:
A run a person starts on a scheduled net outside its cadence: the schedule's next window, fired early. Its success moves coverage and stands in for any scheduled run already overdue; its failure changes nothing.
_Avoid_: trigger run, ad-hoc run (that is a search), side run

### Tables and analyses

Objects added in 1.1 (ADR-0035). They belong to the installation and share one namespace; access comes from grants, never from ownership.

**Table**:
Editable rows with declared, typed, immutable columns and an optional key, stored as one parquet file per revision under a definition Postgres owns. A table is written by people and by nets alike; whoever writes it, it has one definition.
_Avoid_: lookup file, kv store, saved run (that is a run's result), `| table` (that is a projection stage)

**Table key**:
The columns declared at creation that identify a row: a uniqueness and non-null constraint on every write, and the only columns `upsert … on` may name. A keyless table accepts append and replace only.
_Avoid_: primary key, match columns, index

**Revision**:
One version of a table's rows: an immutable file, numbered, made by one write and made visible by one Postgres commit. `from <table>` reads the current one; every write names the revision it saw and fails if it moved.
_Avoid_: snapshot (unqualified), version, epoch (that is the stored format's generation), generation

**Citation**:
A retained run's record of the revision it read. A citation holds the file against retention for the run's lifetime and names bytes, not a type; whether the cited revision still exists is a separate, named fact.
_Avoid_: pin (that is the catalog's type), anchor (that is a link in the runs page), snapshot, reference

**Net**:
A saved query the installation owns: its optional schedule, its retained runs, and the analyses it triggers by version. A net that ends in a save writes a table like any other writer and does not own it.
_Avoid_: saved search, report (that is a run's output), job

**Run**:
One execution of a net: the resolved query text, the window it covered, its retained result, its citations, and a separate outcome for each analysis and save. `run=latest` is the newest run whose query succeeded, never an older one.
_Avoid_: report, execution, receipt (a run carries facts, and there are no write receipts)

**Analysis**:
An installation-owned object that runs a model over rows: instructions, declared output columns, a closed read-only tool set, and an optional save. A net triggers one by version; a person runs one by hand. Its output is data, validated against its declared columns, and can never choose a destination.
_Avoid_: agent, llm call (one analysis makes several), prompt (that is one of its parts)

**Save**:
The terminal write stages, one verb per behaviour: `append`, `upsert … on <key>`, `replace … [key=]`. A save is the last stage, at most one per query, refused in read-only lanes, and one write is one revision is one transaction.
_Avoid_: publish (that is durable file publication, ADR-0026), export, `save mode=`

**Lookup**:
The stage that left-joins a table onto the pipeline on equality: no match keeps the row with NULLs, more than one match fails the query naming the value, and a lookup on the key can never match twice.
_Avoid_: join (the SQL word), enrich (the outcome, not the stage)

**Automation key**:
The one installation-wide key under which scheduled runs execute, checked live at every gate and never an administrator. Revoking it stops all unattended work; disabling a net stops one. It grants nothing to whoever edits a net.
_Avoid_: owner key, service account, the scheduler's key (it is the installation's)

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
