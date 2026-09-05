# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added
- **Scheduler-owned report windows (ADR-0018 rulings 6-14, #107).** A
  schedule now says what its runs cover, instead of leaving it to whatever
  time clause the saved query happened to carry. `PUT
  /api/v1/saved/{id}/schedule` takes `window` and `lag`: `"since_last"`
  tiles, so consecutive runs cover consecutive intervals with no gap and no
  double-count, while a duration such as `"2h"` is a fixed trailing span
  re-measured from every fire. `lag` shifts both bounds back to cover
  events that arrive after the boundary they belong to. A schedule with no
  `window` is unchanged: the saved DSL executes verbatim.

  A `since_last` schedule keeps a watermark, `covered_through`, that
  advances only on success and is seeded at the schedule's origin, so a
  failed first run is healed by the next one rather than dropped. Missed
  runs coalesce into ONE window rather than backfilling N runs, bounded by
  the new `[scheduler] max_catchup_intervals` (default 24, must be at least
  1): past the bound the window start is clamped forward, the run row
  carries `window_truncated: true`, and `trawl_scheduler_window_truncated_total`
  counts it. That counter is the one to alert on, since a truncated run is
  the only case where coverage is permanently missing from the series.

  Each run stores the RESOLVED query text, the saved DSL with
  `earliest="..." latest="..."` spliced in front, so a report reproduces by
  paste. Run rows carry `window_start`, `window_end`, `window_truncated`
  and `window_kind`; all four are absent together for a run that had no
  window and are never backfilled. Every successful run is recorded now,
  zero-row runs included, and a zero-row run is queryable through `| from
  saved <name> run=latest`. A window and a query-owned time clause are
  refused in both write directions with a 400 naming both sides, as is a
  window on a `from saved` query and a `lag` with no window. A manual
  `POST /api/v1/saved/{id}/run` against a windowed schedule is a 409: the
  schedule owns its coverage, and a manual run would either double-count a
  window or advance the watermark past coverage nothing produced.
- **Self-hosted Geist typography (#119).** `fleet-ui` now owns pinned Geist
  1.8.0 and Geist Mono 1.8.0 variable WOFF2 assets, their SHA-256 checksums,
  source record, and OFL-1.1 attribution. The SPA, fleet-ui workbench, and
  generated design cards all load the same 400–700 faces locally; Google
  Fonts links and the corresponding `fonts.googleapis.com` / `fonts.gstatic.com`
  CSP allowances are removed.

- **`repin --to severity`: put a sender's own field on the OTel ladder
  (ADR-0013 ruling 10, #79).** `trawl schema repin level --to severity`
  (`POST /api/v1/schema/repin` with `"to": "SEVERITY"`) retypes one field's
  whole corpus onto the severity ladder, so `level=error` becomes a band
  match, `level>=warn` compares ladder positions, results render tokens and
  live ingest conforms through the same pin from then on. Admission moved to
  the CATALOG parse (`from_catalog`), which is what made the target
  reachable at all; INFERENCE still cannot mint the pin, and the declared
  envelope is refused as a PREDICATE (`schema::is_contract_typed` — the
  whole sealed `_` prefix plus the four sender-asserted names), so a slot
  added later is refused the day it exists.

  **`--dialect otel|syslog`** (persisted on the job row, `otel` by default)
  reads NUMERALS only — words go through the one token table whatever you
  assert. The two ladders overlap over 1-7 with opposite meanings (`3` is
  `trace3` to OTel and `err` to syslog) and no value-shape rule can tell
  them apart, so a corpus carrying them is REFUSED under the OTel reading
  until the operator asserts `syslog` or passes `--force`; the count of such
  rows (`ambiguous_numerals`) is reported whatever the assertion. The
  dialect is carried per ARM: a column that was already `SEVERITY` holds
  canonical ladder positions and keeps its OTel reading, while the `_raw`
  re-extraction — the sender's own wire text — takes the assertion.

  The report gained the evidence a plan's numbers cannot carry: up to five
  distinct `unmapped_samples` of the values the new pin cannot read, a
  `liveness` fact when something is still WRITING the field (a repin
  translates history — the CLI states the cutover discontinuity and points
  at `[ingest] severity_from` for the live half), and **`requires_force`**,
  which every job row now carries. A dry run terminates `succeeded` by
  design, so without that a plan carrying loss or ambiguity read as a clean
  200 and the refusal arrived with the request that was meant to do the
  work; the verdict is computed from the row's own numbers through the same
  decision the two live gates ask, and is ABSENT (not `false`) until the
  scan has recorded a plan.

  **Cost, stated plainly**: the severity rung conforms at roughly 36 µs per
  affected row — about 10× any other target, so ~1 CPU-hour per 100M rows
  carrying the field — with retention suppressed and the affected bytes held
  twice for the job's whole life. The dry run's `rows_carrying` is the number
  to size that against.

- **Producer profiles, configurable derivation sources, and `_producer`
  (ADR-0013 slice 2 rulings 1-6, #75).** All three producers now enter
  through the ONE envelope canonicalizer as *profiles* — `http`,
  `syslog`, `trawld`, a closed set chosen by the server call site that
  owns the transport, never by anything on the wire. The syslog listener
  and the internal-telemetry layer used to hand-roll their own envelope
  maps, skipping `_repairs`, the `_raw` cap, the env allowlist, the
  field-name length cap and the sealed `_`-prefix strip; they now hand
  the door a payload and get every one of those gates. Three latent
  defects close by arriving there rather than by new code: a >255-byte
  `tracing` field name used to reach the WAL and **permanently wedge
  compaction for `service=trawld`**, the syslog lane never applied the
  `MAX_RAW_CHARS` cap to `_raw` (unreachable via today's transports only
  because two unrelated constants happen to be equal — now structural),
  and the syslog batch key ignored the event's `env`.

  **`_producer` is a tenth envelope field** (`http`|`syslog`|`trawld`,
  catalog-seeded VARCHAR), so provenance is queryable data:
  `_producer=syslog | stats count()`. It is server-stamped and
  unforgeable — a `_producer` on the wire takes the ordinary
  reserved-prefix strip and lands under a bare `producer`. Pre-upgrade
  rows simply read `_producer IS NULL`.

  **`[ingest] severity_from` / `time_from`** make the derivation source
  lists config (defaults unchanged: `["severity", "severity_text",
  "level"]` and `["_time", "timestamp", "@timestamp"]`). Entries take a
  bare name or the typed `{ field, dialect }` form, where the dialect
  (`otel`|`syslog`) governs NUMERICS only — which is how a
  syslog-over-HTTP forwarder shipping the raw PRI numeral reaches the
  same inversion the native listener does, through one mechanism rather
  than a privileged writer. Both lists are boot-fatal on a bad entry (a
  list that silently never matches is the sharpest footgun here) and
  **forward-only**: nothing re-derives stored events.

  Two consequences worth knowing. The syslog listener writes **no**
  `_severity` any more — it publishes the raw PRI numeral as
  `syslog_severity` and the frame's own time as `syslog_timestamp`, both
  omitted when the frame carried neither, and the profile's fixed
  sources derive from them; `_severity` on a syslog event means exactly
  what it did before. And a hostile syslog frame or `tracing` field can
  no longer be rejected by these doors, because there is nobody to
  reject to: an unusable APP-NAME lands under the profile's
  `default_service` with `service.from_profile`, a hostname-less frame
  behind a `trusted_relays` peer keeps the event with `host` omitted and
  `host.omitted`, and a payload key colliding with a profile-asserted
  slot loses with `field.producer_asserted`, its value still in `_raw`.
- **Backtick-quoted identifiers, and one rule for aggregate output names
  (ADR-0013 slice 2 rulings 7-8, #78).** Any field name can now be written
  between backticks, and a backticked name is **always** a field
  reference: `` `http-status`=500 ``, `` | table `request id` ``,
  `` | stats count() as `total count` by `where` ``. Backticks change how
  a name is **lexed**, never what a name may be — content is any
  character except a backtick (a doubled backtick escapes one), an empty
  name is a parse error, as is a control or invisible format character
  (bidi controls, zero-widths, the soft hyphen — a parsed name is echoed
  back verbatim in notices and errors), the ASCII fold still
  applies (`` `Dur` `` **is** `dur`), and trawl's `_` namespace is still
  sealed (``let `_foo` = 1`` is the same error as the bare spelling).
  They are accepted in **every** field position, so a name that exists is
  a name you can reach. Function names, stage names and saved-query names
  are not fields and take no backticks: `` `lower`(x) `` is a field
  reference, never a call.

  The search stage's whole keyword set is **three** words — `last=`,
  `earliest=`, `latest=` — and backticks are how you reach fields of
  those names (`` `last`=5 `` beside `last=2h`, in one query). Correcting
  the docs, which claimed one. Two smaller lexical consequences: a `#` or
  `//` inside backticks is part of the name rather than a comment, and a
  LEADING backtick that does not close is now a loud parse error instead
  of a silent search for the literal text.

- **BREAKING — an aggregating stage that would project two columns of one
  name is refused (ADR-0013 ruling 8, #78).** `| stats count() by count`,
  `| stats count() as n, sum(x) as N` (names fold, so those are one
  column), `| timechart span=1h count() by _time` and `| top 5 count`
  parsed and ran before, with the answer depending on which producer the
  engine happened to bind. They are now errors that name **both**
  producers and the way out — `as` where an aggregate is involved, the
  `| stats count() as hits by … | sort -hits | head N` rewrite where
  `top`/`rare` mint their own `count`. One check, both lanes: `/api/v1/query`
  and the SSE stream state the identical sentence.

  **`eventstats` now requires an explicit `as`** (`| eventstats
  avg(duration) as avg_dur by service`): it adds a column to every row,
  and a live tail cannot know a row's schema before the rows arrive. An
  alias naming a column the rows already carry overwrites it, the way
  `let` does. And one auto-name moves: a computed aggregate argument
  names its innermost field in **every** lane now (`avg(tonumber(rssi) *
  -1)` → `avg_rssi`), where the live tail and `eventstats` used to answer
  a bare `avg`. Saved queries, dashboards and alerts carrying any of these
  shapes must be rewritten — no shims, per the project's no-back-compat
  ruling.

- **The severity reading kernel and `sev()` (ADR-0013 slice 2 rulings
  9-10, #77).** "What is this value's severity" now has exactly ONE
  answer: `trawl_core::severity::reading` is THE reader, and the SQL form
  (`conform::severity_reading_sql`) is generated from the same tables and
  probe-pinned against it case by case, in both dialects, against the
  bundled `DuckDB`. Ingest's `_severity` derivation and the live mirrors
  (`compare::conformed_severity`, `pin_match`'s SEVERITY arm) become
  one-line delegates.

  The **`SEVERITY` conform rung widens to that full token-aware reading**
  as the pin's lifetime meaning: a stored `"error"` now conforms to 17
  instead of being shelved as a conflict, live and under a future repin
  alike. Numeric readings narrow to a STRICT integer (`[+-]?[0-9]+`), so
  `"4.0"`, `"1e1"` and `"0x10"` — spellings `TRY_CAST` reads and the
  kernel does not — have no reading in either engine. Everything else the
  reader does is unchanged from ingest's old derivation, deliberately:
  the trim is the full Unicode `White_Space` set on BOTH engines (probed
  character by character), and the token fold stays ASCII on both — the
  SQL gates its token match on an ASCII-alphanumeric subject, because
  `DuckDB`'s `lower()` is Unicode and would otherwise read `"İNFO"` as
  INFO in batch and as nothing live.

  **`sev(x[, dialect])`** applies that kernel at query time to any field:
  `| where sev(level) >= "error"`. It DECLARES its result as the
  `SEVERITY` canonical type (a new one-entry function-result-pin table
  consumed by the pin-scope walk), so the comparison binds through the
  ADR-0011 rule table — equality takes the BAND, an ordered operator the
  token's exact number, a pattern the canonical token text — a `let`
  target adopts the pin and carries it through `stats`, and the result
  column renders tokens. The optional dialect is a literal `"otel"` or
  `"syslog"` governing numerics only; an unknown or computed one is a
  query error naming the vocabulary in the SQL lane and the stream
  compiler alike. Because the pin is DECLARED rather than looked up,
  `sev()` binds identically over embedded `--data` with no catalog at
  all. One classifier (`PinScope::subject_pin`) now roots the emitter,
  the stream compiler and the in-memory evaluator, so the three adopt and
  decline identical shapes.

  `QueryResponse` gains `severity_columns` — a response-level advisory
  beside `degraded_fields`, omitted when empty — naming the result
  columns that render as tokens; `-f json`/`-f csv`/SSE keep the number,
  so arithmetic consumers are untouched.
- **BREAKING — the namespace cutover: two namespaces, observe-don't-consume derivation, zero DSL aliases (ADR-0013 slice 1, #60).**
  One contract, one sentence: **bare names are sender vocabulary trawl
  never assigns meaning to; underscore names are trawl's contract slots.**
  The envelope shrinks from ten fields to nine — `_time`, `_ingested`,
  `_raw`, `_repairs`, `_severity` (trawl-owned) plus `service`, `env`,
  `host`, `message` (sender-asserted). `severity` **leaves the envelope**
  and becomes ordinary sender data; **`severity_text` is deleted
  outright** (it existed to preserve text that consumption destroyed, and
  nothing is consumed now).

  **Derivation observes, it never consumes.** `_time` derives from `_time`
  → `timestamp` → `@timestamp` (first PRESENT wins; only `_time`, the
  proposal slot, is consumed and canonicalized) and `_severity` from
  `severity` → `severity_text` → `level` (first MAPPABLE wins). Every
  source is stored verbatim under the name its sender chose, so
  `{"service":"game","level":"gold"}` — the defect that opened #60 — keeps
  a fully queryable `level` column, gets no `_severity`, and is repaired
  in no way at all. A numeric severity source is read **strictly as OTel
  1-24**: `3` is trace, `0` and `25` map to nothing, and the syslog
  inversion happens only in the syslog listener, where the transport
  proves the dialect. `severity.unmapped` is **deleted** — a derivation
  into the `_` namespace touches nothing sender-visible, so there is
  nothing to confess; the ops signal is the new
  `trawl_severity_unmapped_total{service}` counter.

  **The `_` prefix is sealed at both doors, from one predicate.** At
  ingest, a non-proposable `_x` has its leading underscore RUN stripped
  and its value stored under the bare remainder — `_HOSTNAME` →
  `hostname`, `__name__` → `name__`, `_SYSTEMD_UNIT` → `systemd_unit` —
  with the repair code `field.reserved_prefix`; a bare name the same
  event already carries wins (`field.reserved_prefix_collision`) and a
  key with no remainder (`_`, `___`) is dropped, its value still in
  `_raw`. This ONE rule replaces `RESERVED_CLIENT_FIELDS`,
  **`meta.stripped` (deleted)**, the non-string-`_raw` special case and
  the silent `_trawl_wal_file` removal, and it answers the
  journald/prometheus passthrough that used to lose fields. In the DSL,
  `let _foo = 1`, `rename x as _foo` and `extract "(?P<_foo>…)"` are
  errors: the pipeline cannot mint a name ingest would refuse.

  **Zero DSL aliases.** `level`, `timestamp` and `@timestamp` are ordinary
  field references in every position — including the ones that used to be
  errors (`stats count() by level`, `table level`, `where level in (…)`).
  `catalog_key` is now an ASCII fold and nothing else, and the name you
  type is the column in `DESCRIBE` is the identifier in the SQL, in all
  four lanes. **Severity moves onto `_severity`, which nothing can
  shadow**, via a new `SEVERITY` canonical type (physically BIGINT,
  bounded to 1-24 by its conform rung) riding the ADR-0011 pin rule
  table: `_severity=error` → `BETWEEN 17 AND 20`, `_severity=error2` →
  exactly 18, `_severity>=warn` → `>= 13`, `_severity=warn*` globs the
  canonical OTel token text, and an unknown value is a **query error
  naming the vocabulary** rather than a filter that quietly matches
  nothing. Results DISPLAY the token (`error`, not `17`) in the CLI
  table, the TUI and the web UI; `-f json`, `-f csv` and SSE keep the
  number for arithmetic consumers. Embedded `--data` is pin-blind, so
  compare the ladder number there.

  **Migration is an epoch bump, not a shim.** `data/EPOCH` goes to 3 and
  an epoch-2 root is set aside at boot as `data.pre-epoch-3/` (a second
  name — trawl never deletes a set-aside, so an existing
  `data.pre-schema-v2/` is left exactly where it is). A query-only node
  warns and serves an epoch-2 root instead of moving data it does not
  own. Migration 0010 reshapes only the catalog's `_declared` seed rows,
  so a sender's own `severity` pin survives, and re-arms the boot
  conformance pass.

  **Operational note.** A saved query, dashboard panel or alert written
  `level=error`, `| stats count() by level`, `| sort -timestamp` or
  `| table timestamp` now reads the sender's own column of that name:
  over a corpus that has one it answers a different question, and over a
  corpus that does not it answers with **no rows and no error**. Rewrite
  them as `_severity>=error` and `_time`. trawl issues no advisory about
  it: a retired spelling is an ordinary sender field now, indistinguishable
  from any other name nobody writes, and a notice keyed on it would be
  trawl assigning meaning to a bare name — the exact thing this cutover
  deletes (ADR-0013 §7).
- **SPA operator surface for degraded pins and repin (ADR-0011 slice C2,
  #71).** The degraded-pin case file and the repin trigger leave the CLI
  for the browser, on the schema page's existing anatomy — no new
  top-level view. `GET /api/v1/schema/services` gains a per-service
  `degraded_fields` list (absent when empty, so an older server and a
  healthy install are indistinguishable), loaded beside the degraded set
  on the schema tick and stamped from that in-process snapshot — a
  healthy install pays one probe query and opens no transaction, and
  only an install that HAS a degraded pin pays the additional
  `REPEATABLE READ` re-read that loads both evidence halves coherently
  (a clear landing between two snapshots would publish a degraded field
  with no services attributed to it). It names the fields a service has
  **actually conflicted on**, which is not a client-computable join:
  carrying a degraded field's column is not evidence of having degraded
  it. Service
  rows and the service drawer's fields tab badge from that list, and a
  badged field drills into the **field case file** — `?field=` on
  `/search/schema`, a working deep link with no service context —
  carrying the pin, the verdict facts, a sample of the values the pin
  shelved, paged per-service observations, conflict evidence, and the
  standing statement that a repin rewrites the whole corpus, not the
  service it was reached through. With `schema_write` the case file
  offers a **dry-run-first** repin: the plan (files, rows carrying,
  projected nulls, resurrectable, bytes) before anything is rewritten,
  an explicit force toggle only after a `refused_needs_force` reply,
  status polling that runs only while the case file is open and the job
  is non-terminal, and a completion toast. Without it the same case file
  renders read-only with the equivalent `trawl schema repin` line and no
  disabled affordances — the server stays the sole enforcement. Finally,
  the query notice ships to the browser: `QueryResponse.degraded_fields`
  renders as one dismissible line above the results naming each field,
  linked to its case file under `schema_read` and plain text without it,
  dismissed per (query, field set) so paging the same result keeps it
  away while a new query brings it back. A notice already returned is a
  fact about that execution and is never edited away by a later catalog
  change; the live tail carries none, since SSE has no such stamp. The
  browser surfaces are documented in the new Web UI reference page.
- **Degraded-pin analyzer, evidence and query notice (ADR-0011 slice C1,
  #69).** trawl now concludes that a pin is doing sustained damage and
  says so everywhere the field is read. Conflict evidence gained the
  values it is about to lose — up to five distinct misfit samples per
  conflict row, byte-capped and control-sanitised, captured by
  compaction at null-time — plus durable per-`(field, service)`
  aggregates (first/last conflict, episodes, lifetime rows nulled)
  written in the same transaction, so the 100-row recency window can no
  longer evict the history a verdict rests on. A pure read-time
  **analyzer** calls a pin degraded when its evidence spans ≥24h *and*
  carries volume (≥100 rows shelved or ≥3 episodes); sender count is
  displayed evidence, never a gate. `GET /api/v1/schema/fields` and
  `/schema/field?name=` carry a structured `verdict` (since, senders,
  episodes, lifetime rows shelved, sample values, suggested target type
  — uniform misfit rung, else `VARCHAR`) under the existing
  `schema_read` gate; conflict rows carry their samples;
  `trawl_catalog_degraded_fields` gauges the count. `POST /api/v1/query`
  stamps `degraded_fields` — the degraded fields the query **bound**,
  including ones it filtered on and projected away — refreshed on the
  existing schema tick (SSE and embedded `--data` carry no notice).
  `trawl schema fields` marks degraded pins and counts them, `trawl
  schema field <name>` renders the case file with samples and the exact
  `trawl schema repin` command, and `trawl query` table output appends
  one footer line (json/csv carry the wire field untouched). A
  successful repin clears the field's evidence in the cutover
  transaction, so the badge goes out when the remedy is applied. The
  verdict is advisory and sender-influenceable by construction: nothing
  repins without `schema_write` and a human. Note that `schema_read` now
  exposes fragments of event VALUES — the captured samples — where it
  previously carried names, types and counts only.
- **Operator-triggered field repin (ADR-0011 slice B, #53).** A
  wrongly-pinned catalog field can be retyped to any candidate-ladder
  type with one command: `trawl schema repin <field> --to <type>` /
  `POST /api/v1/schema/repin` (new `schema_write` permission — granted
  to no existing role; `GET /api/v1/schema/repin/status` is
  `schema_read`). The corpus is rewritten as a shadow generation beside
  the data root (unaffected and foreign files hardlinked, affected files
  rebuilt through the conform machinery), **conflict-shelved values are
  resurrected from `_raw`** under the same lossless guard, and the
  cutover is atomic, crash-recoverable (a `data/REPIN` marker replays
  through a boot decision table) and invisible: queries never observe a
  mixed-type corpus, answers are identical before and after, and events
  ingested during the rewrite land exactly once. A mandatory-by-shape
  dry run reports affected files/rows, projected nulls and resurrectable
  values; a lossy repin refuses without `--force` (409 with the plan)
  and accounts its losses as conflict evidence when forced — and
  because ingest keeps running for the whole job, the same gate is
  re-asked of the finished rewrite, so a started job still ends
  `refused_needs_force` (corpus untouched) when data that landed after
  the scan turns out to be unreadable under the new type;
  `--to <current> --force` runs a resurrection-only pass. One job at a
  time install-wide (postgres-enforced), progress and outcomes on
  `trawl_catalog_repin_*` metrics; retention stands down while a job is
  in flight. This retires the documented stop-trawld-and-do-surgery
  escape hatch.

### Documentation
- **`latest=` is an exclusive bound.** The absolute time window is half-open,
  `[earliest, latest)`: an event stamped exactly at `T` matches
  `earliest="T"` and does not match `latest="T"`. It has behaved this way
  since the bounds landed, and is now written down in the DSL reference and
  pinned by two tests: a DuckDB execution probe that runs the emitter's own
  SQL over parquet holding events at `T-1µs`, `T` and `T+1µs`, and a live-tail
  parity test over the same three events. Half-open is the shape that lets
  consecutive windows tile without an event on the boundary being counted
  twice (ADR-0018 ruling 10).

### Removed
- **The orphaned Intel surface is retired (ADR-0020, #112).** The coastwatch
  API it targeted does not exist, and its write controls gated on the wrong
  app's permission, so the whole surface is gone rather than flagged off.
  Removed: the SPA `/intel` pages, their navigation entry and browser API
  client; trawl-web's `/api/intel/v1/{*path}` relay route; the `[web]`
  `coastwatch_url` config key it alone consumed (nothing else read it —
  `[web]` takes no `deny_unknown_fields`, so a deployed config still carrying
  the key keeps parsing and the key is simply ignored); the hand-vendored
  `coastwatch-api-types` crate; and the Intel-only rules in `main.css`.
  Any request under `/api/intel` now returns the JSON 404 the retired
  namespace shares with the rest of `/api/*`. Git history is the archive:
  re-commissioning starts from ADR-0020, not from the removed stand-ins.

### Changed
- **Authenticated permission denials now return 403 (#120).** Missing,
  malformed, invalid, expired, and revoked credentials remain opaque 401s.
  A valid key that lacks a route permission, including ingest permission or
  ownership of a query it tries to cancel, now receives the existing
  `forbidden` error code with the existing safe denial message.
- **Public health checks no longer disclose backend diagnostics (#120).**
  `/api/v1/health` keeps its four component keys, overall status, HTTP status,
  and metrics. Each component value is now exactly `ok` or `error`; database
  errors, task errors, DSNs, and filesystem paths do not enter the response.
- **BREAKING: `trawl-web` requires `[web] public_origins`, and the origin
  check compares the whole origin (ADR-0016, #92).** The CSRF guard used to
  compare the `Origin` header's *host* against the request's `Host` header.
  That admitted same-host cross-scheme and cross-port forgery (an active
  attacker serving `http://trawl.example.com`, or another port of the same
  name, passed) and it made the verdict depend on a header a reverse proxy
  rewrites. A present `Origin` is now compared whole, normalized scheme and
  host and effective port, against origins the operator states, and no
  forwarding header is consulted from any peer.

  **Every trawl-web install must state its browser-visible origin before
  restart**: `[web] public_origins = ["https://trawl.example.com"]` in
  `trawld.toml`, or `FLEET_SESSION_PUBLIC_ORIGINS` as a comma-separated
  list. An empty list is a startup error naming the knob. There is no
  host-only legacy mode and no empty-means-allow-everything default,
  because both fail silently. State what the *browser* shows, not the
  loopback address a TLS-terminating proxy forwards to, and state every
  spelling you serve: `http://localhost:8090` and `http://127.0.0.1:8090`
  are two origins, as are `https://x` and `https://x.` (the DNS root dot a
  browser preserves). Only the default port normalizes, so `https://x` and
  `https://x:443` are one.

  The Debian package ships the two loopback spellings of its own packaged
  bind, so a default install starts as it did. Helm refuses to render
  `web.enabled=true` without `web.publicOrigins`, never derives it from
  ingress hosts or httpRoute hostnames, and also passes the list to the
  sidecar as `FLEET_SESSION_PUBLIC_ORIGINS` so a `config.raw` replacing the
  generated TOML still carries it.

  The check reaches further than it did. It runs in the session extractor
  instead of at individual handlers, so it now covers `/api/v1/stream` and
  `/api/v1/dashboard/stream` (both SSE routes bypassed the old guard
  entirely), every proxied method including GET, and `/api/auth/me`. It is
  decided before the cookie is read, so a disallowed origin with an expired
  cookie is a 403 with no `Set-Cookie` rather than the expiry branch's clear
  directive. Bearer clients stay exempt (nothing a foreign page does makes a
  browser attach someone else's `Authorization` header), and an absent
  `Origin` still passes, so curl, the CLI and vector are unaffected.

  Sibling fleet apps adopt the fleet-auth API change in their own PR:
  coastwatch is red against the path dependency until it does, and it will
  need its own `public_origins` list.

- **The scheduler fires on planned boundaries (#107).** A schedule carries a
  fire cursor, `next_fire_at`, and a tick claims the latest boundary at or
  before its sampled instant. Runs used to be due on elapsed time since the
  last run STARTED, so execution time and poll jitter walked the schedule
  forward: a 90-second run on an hourly cadence drifted a minute and a half
  per fire. Boundaries missed while trawld was down are folded into one
  catch-up window instead of replayed. `next_fire_at` is on the schedule
  response, re-anchored to now when the interval or the window changes and
  left alone when only `max_runs` or `enabled` does.
- **`now()` is one instant per unit of output (ADR-0017 §3, #106).** It used
  to be read per CALL SITE: a `let` and a `where` in one statement could see
  different instants, and the streaming lane sampled its filter window once
  per bus batch and its pipeline once per event, so a single event could be
  admitted by one clock and evaluated against another. The instant is now
  captured once per unit of output and handed to every reader.

  Three observable changes:

  - **`typeof(now())` is `TIMESTAMP` in batch**, where it used to be
    `TIMESTAMP WITH TIME ZONE`. The batch lane no longer emits SQL's own
    `now()`; it binds the captured instant as a TIMESTAMP parameter under an
    explicit cast, so the type is trawl's answer rather than the driver's
    parameter inference. Comparisons and `strftime`/`tostring` renderings
    are unaffected — the session is UTC either way — but an expression that
    read the *spelling* changes.
  - **Two `now()` reads in one statement are equal, in every lane.** Batch:
    one instant per logical query invocation, inherited by a retry, by the
    hot-only cold-start fallback and by the `rust_stages` tail behind
    `extract kv`. Live pass-through: one per event, so each row is frozen
    while successive rows advance, and the search stage's
    `last=`/`earliest=`/`latest=` window reads that same per-event instant.
    Aggregate streams: every row of one emitted snapshot shares one instant,
    while the events that fed it each sampled their own. The batch `last=`
    window is deliberately still DuckDB's own statement clock — a separate
    clock domain, read at its own moment, with no bound on the gap between
    the two reads.
  - **Aggregate SELECT parameters bind in rendered order.** DuckDB binds `?`
    positionally, and the emitter walks the search stage before the SELECT
    list it renders first, so any parameter a `stats`/`timechart` aggregate
    pushed collided with a predicate's. This predates the anchor and hit
    ordinary literal arguments — `stats max(substr(message, 1, 3))` — as
    well as `stats max(now())`, and the type-compatible collisions swapped
    SILENTLY (an `earliest=` bound answered as the aggregate). The emitter
    now flushes the pending predicate into a CTE exactly when an aggregate
    appended a parameter, leaving `stats count() by host` unnested.
  - **A second `pivot` reads the first one's output.** A pending pivot is
    flushed to a CTE before any following stage, and that used to exclude
    another `pivot` — but the pivot stage opens with an ordinary flush
    that does not render a pending `PIVOT`, then overwrites it. So
    `pivot count() on status by host | pivot sum(status) on status by host`
    silently returned the SECOND pivot alone, computed over pre-pivot
    rows. Consecutive pivots now compose, and naming a column the first
    pivot consumed into dynamic columns is a loud `unknown field` instead
    of a quietly different answer.
  - **A `head`/`sort` before an aggregating stage now applies to the
    aggregation's INPUT.** SQL applies LIMIT and ORDER BY *after* an
    aggregation, and both clauses used to stay on the same SELECT, so
    `service=nginx | head 2 | stats count() as c` counted every matching
    row and then limited the one-row result. `timechart` did the same, and
    `top`/`rare` — which desugar to a `count()` aggregation — absorbed the
    pending clauses into the aggregation too, so `head 2 | top 3 host`
    counted all six matching rows and then kept two GROUPS. Every
    aggregating stage now flushes whenever either clause is pending, which
    also stops the answer depending on a SIBLING: before this,
    `stats count() as c` and `stats count() as c, max(now()) as n`
    disagreed, because only the second bound a parameter and got flushed
    for that reason.

- **BREAKING — comments are a grammar production, and `//` is no longer a
  comment (ADR-0014, #83).** The pre-parse comment scanner is deleted.
  It blanked `#`/`//` runs to end-of-line before the grammar ran, which
  meant it had to re-derive the parser's own decisions from outside the
  parser — and it got them wrong on ordinary shapes, *successfully*:
  `message=/a#b/` executed as `message == "/a"`, `url=https://example.com/x`
  as `url == "https:"`, and `referrer=https://a.b/c status=200` **deleted
  the `status=200` filter** and widened the result set. No error, no
  warning.

  A comment is now `#` to end of line, admitted only where the grammar
  sits between tokens — the start of the input, or after whitespace. Two
  breaking halves, both loud:

  - **`//` stops being a comment opener.** A value carries it freely and
    needs no quoting (`url=https://example.com/x`, `path=/api//v1`,
    `url=//cdn.example.com/x`), and the sibling filters on the same line
    survive. The loud half: a bare search **term** that *starts* with
    `//` is a parse error naming `#`, so an existing `// note` line fails
    instead of silently becoming AND-ed text terms that match nothing.
  - **A `#` inside an unquoted token is a parse error**, never data and
    never a comment: `foo#bar`, `color=#ff0000` and `a=1# note` each
    report one error spanning the `#` byte, with a hint naming the quoted
    form. Quote it to include it — a double-quoted value, a
    backtick-quoted name and a regex body all carry `#` verbatim, because
    none of them has a place inside where the grammar skips whitespace.

  `a=1 # note` is unchanged. Every shape carrying a comment opener moves
  to a correct answer or a loud error — none of them changes quietly.
  One narrow class DOES change quietly, and it carries no opener at all:
  a filter list may now be written with quoted elements in any position
  (see below), so four shapes that used to read the quotes as data now
  read them as quoting. `a="x",y` and `host="a b",c` were an equality
  plus a separate bare text term `,y` / `,c`, and are now the IN lists
  `a IN ("x","y")` / `host IN ("a b","c")`; `a=1,"b"` and `a=1,"b",c` had
  literal `"` characters in their list values, and no longer do. The
  widening is deliberate — it is what lets `format` quote a list element
  that would otherwise re-lex as something else — but it is a silent
  change, not a loud one. Parse-error spans are now natively correct — the scanner's
  unstated byte-length-preserving invariant (which the web UI squiggles,
  the TUI caret regions, the CLI caret renderer and the wire `ErrorSpan`
  all leaned on) is retired rather than maintained. The DSL reference
  gains the Comments section it never had.

  Two smaller consequences: the formatter no longer quotes a value for
  carrying `//`, and a filter list element may now be written quoted in
  any position (`status=200,"a#b",301`), which is what lets a list
  round-trip through `format`. The web UI's date-range walk consumes a
  scan primitive exported from `trawl-core` instead of its own copy, and
  now locates a `last=` clause correctly in a query carrying `//`.
- **A severity list writes its subject once per contiguous range (#82).**
  Every `SEVERITY`-pinned equality, `!=` and IN now expands to the ladder
  points it accepts and emits the **minimal contiguous ranges** covering
  them, instead of one `BETWEEN` per named band. `_severity=error` is
  unchanged (`BETWEEN 17 AND 20`), but `_severity=warn,error` collapses
  from two OR'd ranges to a single `BETWEEN 13 AND 20`, and all six base
  bands collapse to one `BETWEEN 1 AND 24`. Matching is unchanged in every
  lane — the expansion is exactly each band's own `lo..=hi`, and the
  points/ranges/live-membership agreement is drift-guarded exhaustively.
  What changes is cost: the subject of a `sev(field)` comparison is over a
  kilobyte of SQL, and a six-band query used to repeat it six times. Probed
  over 1M rows, the six-band case goes from ~29.5 ms to ~5.0 ms (5.9x).
  Range bounds are inlined `i64` from the closed ladder table, so a
  band-or-list severity filter binds no parameters at all. A scalar exact
  comparison (`_severity=17`) is unchanged and still binds its one value
  as `= ?`.

  Recorded because the first attempt shipped the wrong shape: collapsing
  the bands into `IN (17, 18, 19, 20)` makes the SQL 5.7x smaller and the
  query **3.7x to 66x slower** — `DuckDB` takes an `IN` list over a
  computed left-hand side off its fast path. SQL text size was never the
  cost. The probe that establishes this is committed and `#[ignore]`d at
  `crates/trawl-engine/tests/severity_set_bench.rs`.
- **Severity presentation metadata is computed inside the query permit
  (#79).** `severity_columns` on `/api/v1/query` is now decided by the
  executing task, under the catalog snapshot the rows were produced with,
  instead of by a second snapshot the handler took before execution. That
  coherence used to be free — a `SEVERITY` pin could be neither created nor
  destroyed at runtime — and `repin --to severity` makes it earnable. The
  walk (a regex compiled per `extract` stage, so client-shaped cost) also
  moves off the reactor thread to where `max_concurrent` bounds it and the
  query timeout covers it.
- **A `| from saved` query's severity presentation is rooted in an EMPTY
  catalog (#79).** The DSL after `from saved` runs over a saved run's stored
  parquet, and that stage's pin scope clears — so a column in those rows is
  no longer typed by whatever this corpus happens to pin now. Machine
  formats are unaffected (the number is what every wire format carries);
  this is a token-rendering change only.

### Fixed
- **`PUT /api/v1/saved/{id}/schedule` honours `enabled` on create (#107).**
  The flag reached the update path only; the create path's INSERT hardcoded
  it to true. A `PUT {"interval": "1h", "enabled": false}` on a saved query
  with no schedule yet therefore created an ENABLED schedule, which the next
  tick claimed and ran.
- **`run=latest` and `run=N` no longer skip past a zero-row success (#107).**
  A successful run that found no rows wrote no parquet and no blob, and the
  lookup filtered such rows out, so `| from saved <name> run=latest`
  answered from an OLDER run: a superseded window presented as the current
  report, with nothing on the wire to say so. A zero-row result is now
  persisted as a compressed JSON blob holding its column names, which
  resolves to an empty typed source that downstream stages bind against
  (`stats count()` over it answers 0). `run=N` resolves a run exactly as
  `run=latest` does, and neither falls through to an older run: a run whose
  parquet write failed and left rows in the blob is a 409 naming the run,
  and a success with neither file nor blob is a 500. `run=all` keeps
  unioning files only, so `_run_id` never names a run that found nothing.
- **The streaming evaluator answers what the query engine answers (#105).**
  Every DSL expression runs in two lanes — `DuckDB` SQL for `/api/v1/query`,
  an in-memory evaluator for the SSE live tail and for the `rust_stages`
  batch tail behind `extract kv` — and the evaluator had grown value-domain
  rules of its own. It has none now: each answer that depends on how `DuckDB`
  parses, casts, compares or renders a value goes through one probe-pinned
  owner, and every rule below is executed against the bundled engine in
  `trawl-engine/tests/duckdb_probe.rs` rather than reasoned out (ADR-0017).
  A live alert and the query you wrote it from now agree event for event.

  This changes answers. In rough order of how likely you are to notice:

  - **`/` is true division.** `status / 100` over a `404` is `4.04`, not `4`,
    and the result is a DOUBLE whatever the operands were.
    `floor(status / 100)` gets the old value back — as a DOUBLE `4.0`,
    since `floor` widens too. Division by zero is
    IEEE — `1.0 / 0` is `inf`, `0.0 / 0` is `NaN` — where the evaluator used
    to answer NULL; `%` follows `/` whenever either side is a float
    (`5 % 0.0` is `NaN`), while integer `% 0` stays NULL.
  - **`+`, `-`, `*` overflow to NULL in the live lane.** `DuckDB` raises an
    `Out of Range` error for one, and an SSE subscription cannot raise a
    per-event error without dying, so the evaluator nulls where batch errors
    (ADR-0017 §4). It used to PANIC in a debug build and wrap silently in a
    release one.
  - **`if()` and `case()` take `DuckDB`'s boolean cast, not truthiness.** A
    string reads through the closed vocabulary `true`/`t`/`yes`/`y`/`1` and
    `false`/`f`/`no`/`n`/`0`; **any other string nulls the whole call** where
    a non-empty one used to take the THEN branch, `" true "` included (the
    cast does not trim). Numbers are non-zero, `NaN` is true, a timestamp or
    a list has no reading. `and`/`or`/`not` and the `where` gate keep the
    older, wider predicate on purpose — they decide whether a live alert
    fires, and this release was not licensed to change that.
  - **`ceil`/`floor` return DOUBLE** over an integer argument too (`ceil(5)`
    is `5.0`); `round` keeps an integer argument integral. `typeof` of an
    integer is `BIGINT` (the emitter binds literals as parameters, so
    `INTEGER` was a spelling the batch lane never produced), and a wire
    integer above `i64::MAX` reads as `DOUBLE` in an expression.
    `tonumber(true)` is `1.0`, not NULL.
  - **NaN compares in `DuckDB`'s TOTAL order**: every NaN equals every other
    NaN and outranks every finite value, and the two zeros tie. This also
    fixes a live filter — a DOUBLE-pinned `metric=nan` matched in batch and
    matched nothing on the live tail. A NaN also RENDERS with its sign now
    (`-nan`), which is the text a DOUBLE-pinned glob matches.
  - **Timestamps read through one owner.** The evaluator's own parser is
    gone: a text coerces through the same `TRY_CAST` a bound parameter gets,
    so a malformed offset (`+ab:cd`) has no reading instead of being stripped
    unvalidated, and a text with no reading makes the comparison UNKNOWN —
    the lexical string fallback is withdrawn (ADR-0017 §2), so `t < "zzz"`
    is unknown rather than true, in both operand orders. `infinity` and
    `-infinity` became values rather than parse failures: they order below
    and above every date, render as their words, pass through `date_trunc`
    unchanged, and null `date_part`/`date_diff` exactly as the engine does.
    `date_part("epoch", …)` is the microsecond count divided once, so a
    far-future instant no longer answers `253402300799.00003` where the
    engine answers `253402300799`. `%f` is a six-digit MICROSECOND field in
    both lanes (it was chrono's nine-digit nanosecond count); the fixed width
    leaves one under-read on the way in, where `DuckDB` accepts a fraction of
    any length. `x in (…)` is three-valued: an element that answers UNKNOWN
    makes a non-match UNKNOWN rather than FALSE, which used to invert under
    `NOT`.
  - **`json_extract` returns JSON TEXT**, as the engine does: a string
    keeps its quotes (`"x"`), a number/boolean/`null` is its own text, and
    an array or object is compact JSON. It used to decode — `1` came back
    as an integer and an array as a list `tostring()` nulled. Reach for
    `json_extract_string`, unchanged, when you want a string's contents.
    One residual, on numbers of exotic magnitude: the streaming path
    re-renders from `f64` where the engine renders the source spelling, so
    `1e16`…`1e20` and integers wider than 64 bits can come back spelled
    differently.
  - **Rows carry typed cells between stages.** They used to cross each stage
    boundary as JSON, which cannot spell a non-finite double — so
    `| let x = 0.0 / 0 | where x == x` kept the row in batch and dropped it
    live, and the `extract kv` batch tail received a NULL where its own SQL
    prefix had computed `1.0 / 0`. That tail now counts, compares, aggregates
    and prints those values; a computed special survives a stage boundary
    live; group/`dedup`/`top`/`rare`/`values()` keys render floats through
    `DuckDB`'s own text (`1e-7` reads `1e-07`); `-0.0` groups with `0.0` and
    a NaN group key renders `nan` rather than JSON `null`;
    `min`/`max`/`median`/percentiles and the batch tail's `sort` order
    through the same total order, so `max()` no longer silently skips a NaN;
    and whole-row `dedup` key BYTES changed (cells are kind-tagged now that
    JSON quoting no longer tells a string from a number) while the
    equivalence classes did not. A number above `i64::MAX` keeps its digits
    through all of it — on the wire, in a `dedup` key, in a group — while
    still computing as the double it always did.

  **The wire is unchanged.** JSON has no spelling for an infinity or a NaN,
  so the API, `-f json` and the SSE stream still render one as `null` — once,
  at the edge, in both lanes. `-f table` and `-f csv` print `inf`/`-inf`/`NaN`
  under embedded `--data`, the one path that does not cross JSON.

- **A quarantine no longer destroys the previous forensic artifact (#115).**
  Compaction moves a corrupt WAL or parquet file aside as `<path>.corrupt`,
  but `rename` silently replaces an existing destination — and the paths that
  corrupt are recurring ones (`{env}/{date}/{HH}/{service}.parquet` is stable
  across ticks), so a second corruption erased the first one's bytes. The
  destination is now reserved with `create_new` before the rename and a taken
  name yields `<path>.corrupt.1`, `.corrupt.2`, …; every artifact is kept, all
  of those names stay inert to the scan globs, a failed quarantine is still a
  hard error, and a rename failure triggers best-effort reservation cleanup;
  a failed cleanup leaves a zero-byte artifact and warns
  `quarantine_reservation_stranded`.
- **A repin retypes `/api/v1/schema` immediately (#115).** The unscoped column
  listing is TTL-cached (`schema_cache_ttl_secs`, 60s by default), so after a
  repin cutover the endpoint that feeds autocomplete kept advertising the OLD
  type for up to a full TTL while queries already answered under the new one. A
  cache entry now carries the pin generation it was built under, and the
  generation is bumped by the cutover's pin flip — the listing is invalidated
  the moment the corpus is retyped, without the repin engine reaching into the
  cache.
- **The query planner has no path left to the `**` glob (#115).** The
  parse-failure exit in source computation returned `{base}/**/*.parquet`, the
  one glob that reaches past the env dimension into `scheduled/`. Nothing read
  that value — the executor parses the same DSL text before it reads, and
  rejects it — so the exit now takes the same no-match shape as every other
  "nothing to read" exit and the fallback parameter is gone from the function
  entirely. With no exit able to produce a recursive glob, the query debug log's
  always-false `is_fallback` field is removed.
- **BREAKING — a backtick ends every unquoted position, and a name can be
  quoted after an operator (#78 follow-up).** Backticks were never value
  quotes, but a query that used them as such answered something quietly
  narrower: `` service=`my service` `` parsed as a filter for the literal
  text `` `my `` beside a stray search term, and `` host=`a # b` more ``
  turned a comment's own words into AND-ed search terms. A backtick now **ends** an
  unquoted value and an unquoted word alike, so every one of those shapes
  is a loud parse error, and `` -`http-status`=500 `` — a negation the
  search grammar has no production for — is one too (write
  ``NOT `http-status`=500``). Text that genuinely contains a backtick is
  double-quoted: `` host="a`b" ``, `` "er`ror" ``, which carry it verbatim.

  With no unquoted position able to absorb a tick, the pre-parse comment
  scanner can open a quoted name after an arithmetic operator as well:
  `` | sort -`a#b` ``, `` | sort -`http://x` `` and `` | let x = 1+`a#b` ``
  parse instead of dying as unterminated names, so a field whose name
  carries a `#` or `//` can be sorted descending and used in expressions —
  the contract `quote_dsl_field` already promised. `/` deliberately stays
  out of that set: it opens a regex far more often than it divides.
- **The web UI's field drill-in works for a field named `count` (#78
  follow-up).** Expanding that row in the service drawer composed
  `| top 10 count`, which projects the field beside a `count` column
  `top` mints itself — two columns of one name, so the projection
  collision check introduced with backtick identifiers answered 400. The
  drawer now composes the aliased stats form for that one name and reads
  the counts back from the column it actually asked for; every other
  field keeps `top 10`.
- **Query formatting no longer changes what a filter value means (#78
  follow-up).** `/api/v1/validate`'s `formatted` field and the TUI/web
  editors' reformat button render a filter value bare whenever the bare
  grammar can spell it — but a value position re-lexes more than that.
  `host="a#b"` and `host="a//b"` came back as a filter for `a`, because
  comments are stripped before the grammar runs; `host="/foo/"` came back
  a regex and `host="a*b"` a glob, so an exact match — or a `!=` — turned
  into a pattern. Those shapes are now quoted, and nothing else is: a
  value with an inner slash (`/foo/bar/`), a genuine glob (`path=/api/*`)
  and a genuine regex still render exactly as before.
- **Backtick identifier follow-through after #81.** Catalog names are now
  declined when the DSL cannot represent them and otherwise flow through the
  shared renderer in every TUI/SPA query builder; URL facet state carries
  arbitrary names in a versioned opaque payload. Completing an open quoted
  name no longer doubles its opening tick, and non-ASCII prefixes use
  character rather than byte offsets. Query formatting now round-trips
  backticks, whitespace and backslashes in values. Streaming `count(1)` is
  restored as the row-count operation while unsupported computed aggregates
  remain loud errors.
- **Projected field names now bind identically in batch and live pipelines
  (#78 follow-up).** DuckDB treats identifiers as ASCII-case-insensitive;
  live rows now use that same binding for every downstream field read, and
  `let`, `rename`, regex extraction, and KV extraction replace any existing
  case-variant of the column they write. Regex extraction also writes NULL
  on no match, a missing optional group, or an empty capture, matching its
  emitted `nullif(regexp_extract(...), '')` expression. A `let`/`eval`,
  `rename`, or regex capture list that names one folded target twice is
  refused instead of relying on DuckDB's suffix-based deduplication.
- **A function call in an aggregation position no longer panics the
  emitter (#77, landed unclaimed in #80).** `stats split(message, ",", 1)`
  — any multi-argument scalar call where the pipeline expects an
  aggregation — hit an unguarded index in `translate_function` and took
  the whole query down with it (mitigated to a 500 by the panic layer,
  but a panic all the same). The `sev()` work unified argument emission
  into one walk shared by every call position, which closed the hole;
  recorded here because the fix shipped as a side effect of a refactor
  commit and deserves a paper trail.
- **`service=X last=Nh` no longer answers 200 with zero rows over an idle
  hot buffer (#73).** A time-filtered query becomes a *list* source — one
  glob per hour in range — and `read_parquet` rejects the whole list when
  a SINGLE element matches nothing. Hour directories are created by
  whichever service compacted into them first, so on any install running
  more than one service the list named files the queried service never
  wrote. Two of the four read lanes carried a post-failure retry for
  that; `run_query` and `export_parquet` carried none, so the moment the
  hot buffer was empty — an idle minute is enough — the query returned an
  empty success and the export a 500. List-source resolution is now a
  property of the source, settled once before the read on every lane —
  after the hot-buffer snapshot is taken, so the two halves of the union
  can never disagree about which files exist. Two visible consequences: some answers that
  were silently empty are now a retryable 503 `cold_data_unread` (the
  read could not reach files the source still points at — retry), and an
  export that raced a file move returns that 503 instead of a 500. A
  genuinely empty time window still answers 200 with zero rows, and an
  export over one still surfaces the underlying error — there is no empty
  parquet for it to write.
- **Embedded parquet export writes a file again (#113).** `trawl query
  --data <glob> -f parquet --output <path>` asks for every row, and the
  unbounded sentinel reached DuckDB as a literal `LIMIT
  18446744073709551615` — outside its INT64 LIMIT domain, so the export
  died with a conversion error before writing a byte. An unbounded
  export now emits no LIMIT at all, and so does any cap past `i64::MAX`
  (`[server] max_export_rows` is operator-set, so the sentinel was never
  the only way to name one); a cap DuckDB can name still applies exactly
  as before. The same clamp covers reading a scheduled run back from its
  stored parquet.
- **A multiline value survives PIVOT finalization (#113).** When a stage
  follows `| pivot`, the pivot becomes a CTE and its parameters are
  inlined as SQL literals. The CTE body was then indented line by line,
  which pushed two spaces after EVERY newline — including the ones
  INSIDE a string literal — so `message="a<newline>b" | pivot count() on
  status | sort ...` searched for `a<newline>  b` and quietly matched
  nothing. Indentation is now quote-aware, like the parameter inliner
  beside it: a newline inside a `'…'` literal or a `"…"` identifier is
  data and is copied verbatim, and both scans read ONE transition rule.

### Changed — behavior
- **`| where` and `| let` comparisons follow the field catalog's pins
  (ADR-0011 slice A′, #66).** Bare field-vs-literal comparisons in the
  pipeline stages now consult the same pin snapshot and the same rule
  table the search stage adopted in slice A — in batch SQL, live tail
  (SSE), and the post-`extract kv` batch tail alike. Concretely: over a
  VARCHAR-pinned field, `| where status > 400` stops raising a
  Conversion error and starts filtering in `DECIMAL(38,6)`;
  `| where status == 200` gains the numeric arm and now matches a stored
  `"200.0"`; `| where status in (…)` routes each element through the
  equality rule; `matches`/`like`/`ilike` against typed pins match the
  stored value's canonical text (on TIMESTAMP pins, live-tail ordered
  comparisons become the instant comparison batch always performed,
  instead of lexical text). Which pin applies follows the pipeline:
  `rename` remaps it, a computed `let` removes it (a bare alias copies
  it), aggregations keep group-by keys only, `extract kv` passes the
  scope through. Quote provenance is discarded (`where status == "400"`
  is `where status == 400`). The pipeline `!=` keeps plain SQL null
  propagation — no `OR field IS NULL` widening — so a repin never
  changes missing-field semantics. Field-vs-field, function-wrapped and
  arithmetic comparisons, unpinned fields, and embedded `--data` mode
  are byte-for-byte unchanged. The post-`extract kv` tail now evaluates
  over the stored **UTC** instant with the display-zone shift applied
  *last* (the all-SQL path's order — previously the tail compared
  display-shifted text, skewing every timestamp comparison by the
  client's offset), and the final shift follows the tail's own lineage:
  a timestamp column `rename`d or copied by a bare-alias `let` inside
  the tail still renders in the display zone, while a *computed* value
  (`let t = coalesce(_time, x)`, aggregate outputs) is the tail's own
  and renders UTC. The envelope seed pins
  `host`/`service`/`env`/`message`/`severity_text`/`_raw` VARCHAR, so
  this is live on day one of every install. Unblocks the repin engine
  (#53).
- **Sibling references inside one `| let` resolve column-first,
  alias-second (ADR-0011 slice A′, #66).** The in-memory stage — live
  tail (SSE) and the post-`extract kv` batch tail — now mirrors what
  DuckDB does with the single projection the batch lane emits
  (`COLUMNS(c -> c NOT IN (targets)), (expr) AS tgt, …`): a target
  naming a column the row already carries stays invisible to its
  siblings, so `let a = 1, b = a` and `let a = a + 1, b = a` give `b`
  the **original** `a` where the previous sequential evaluation handed
  it the just-computed one; a target the row does *not* carry — the
  ordinary case, since `let` usually names something new — is the
  lateral column alias a later sibling reads, so
  `let ms = 1000, total = ms * 2` answers `2000` in every lane, as it
  always did in SQL. Which column a name binds is DuckDB's own
  case-insensitive rule (`let A = 1, b = A` shadows an `a`), and a
  pipeline field *read* now binds the same way whether or not the field
  is pinned, so a reference does not change meaning with the pin. Pins
  never follow the alias — an alias-bound sibling is unpinned in both
  lanes. One residual: a column the *corpus* carries but *this row*
  leaves absent (a sparse custom field) is a NULL column read in batch,
  while the live lane, seeing no key, binds the alias.
- **Comparisons follow the field catalog's type pins (ADR-0011 slice A, #63).**
  Search-stage field filters — batch queries, exports, *and* live tail (SSE) —
  now consult the field's pinned type instead of guessing from the query
  literal. Against a VARCHAR-pinned field, `status=200` / `status!=200` /
  `status=200,301` compare **as text** (matching the stored `"200"`), with a
  numeric literal additionally matching any spelling of the same number
  (`"0200"`, `"200.0"`) — the text a number is stored under depends on the
  batch it arrived in, so the reading is what keeps live tail and `/query`
  answering alike — and `status>=400` compares **numerically** in
  `DECIMAL(38,6)`, the same space on the column and on the literal (see the
  exact-integer entry below) —
  `"404"` matches, `"accepted"` quietly doesn't, and nothing errors where the
  pin-blind emission previously threw a Conversion/Binder error. Glob and
  regex against numeric/boolean pins match the **stored** value's text
  form (`status=4*` finds 404 in a BIGINT column; previously a hard
  error) — which is not always how the event spelled it: a wire `"0404"`
  is stored as the integer 404 (so `status=0*` matches nothing),
  `"accepted"` under an integer pin is stored as NULL, a **boolean** pin
  holds only the values DuckDB writes back — the lowercase words `true`
  and `false`, so `flag=/^true$/` matches those and `flag=TRUE*` matches
  nothing (a wire `"TRUE"`/`"t"`/`"yes"`/`"1"` does not survive the
  conform's round-trip guard and is stored as NULL) — and for a **double**
  pin that text is DuckDB's own
  double rendering, which always carries a fraction and a signed
  two-digit exponent (`200.0`, `1e-07`), so `dur=/^200$/` matches nothing
  on either side while `dur=/^200\.0$/` matches both — and
  against a **timestamp** pin they match the RFC 3339 UTC-microsecond form
  of the stored *instant*, offsets applied
  (`_time=/T09:/`, `_time=/\.123456Z$/`; see the zone-aware entry below) —
  the same text batch and live, not DuckDB's space-separated rendering.
  **This is live on day one for every install**: the envelope seed pins
  `host`/`service`/`env`/`message`/`severity_text`/`_raw` as VARCHAR, so e.g.
  `host=42` changes from a potential Conversion error to a clean text match
  immediately — strictly a fix, but saved queries that leaned on implicit
  casting may match differently. Unpinned fields, embedded mode (`--data`,
  no catalog) and the pipeline `| where` stage keep today's literal-driven
  *coercion* — how the literal binds is unchanged there — but the live-tail
  NULL rules in the next entry change for **every** query, pinned or not.
  Both are documented in the DSL reference. This lands **before** the repin
  engine (#53) so a future type repin changes storage, not query meaning —
  in the search stage. The pipeline `| where` / `| let` stages are still
  pin-blind, so that promise is not complete until they follow; ADR-0011's
  amendment sequences that slice ahead of the engine.

- **Live tail (SSE) evaluates the search stage in SQL's three-valued logic
  — two NULL reversals, on unpinned fields as much as pinned ones
  (ADR-0011 slice A, #63).** The in-memory matcher behind
  `GET /api/v1/stream` used to collapse "the event has no such field" to
  *false* and then let `NOT` negate that into a match. It now answers
  UNKNOWN, like the NULL column it mirrors, and UNKNOWN propagates through
  `NOT`/`AND`/`OR` by SQL's rules. Pins are what surfaced this, not what
  caused it: each direction was already a live/batch divergence against SQL
  that has emitted `("f" != ? OR "f" IS NULL)` and a bare `NOT (...)` since
  long before this release, so the fix is live tail agreeing with the batch
  answer operators were already getting from `/api/v1/query`. Two live-tail
  behaviors flip, in opposite directions:
  - `f!=x` now **matches events that carry no `f`** (and events whose `f`
    is JSON null). Bus events carry the envelope plus whatever their sender
    sent, so a live tail keyed on `!=` over a sparse custom field can go
    from a trickle to most of the firehose. Pair it with `f=*` (or filter
    on a field the events actually carry) to get the old shape back.
  - `NOT f=x` — and any negated field filter, `NOT level=...` band, or
    `NOT <bare term>` — **no longer matches events that carry no `f`**,
    because `NOT (NULL)` is NULL and the batch query has always dropped
    those rows. **A live alert written as `NOT f=x` to catch events missing
    `f` stops firing**; use `f!=x`, whose emitted form is the total one.

- **The hot window now matches what compacted storage holds (ADR-0011, #63).**
  A freshly ingested event was conformed to its pin with a bare `TRY_CAST`
  while it sat in the hot buffer, and with the catalog's lossless round-trip
  guard once compaction wrote it to parquet. The two disagree in exactly the
  cases the guard exists for: `"1.5"` under a BIGINT pin read as `2` for the
  minutes it was hot and NULL thereafter, `"TRUE"` under a BOOLEAN pin read
  as `true` and then NULL. So `dur=2` was a hit, then a miss, with nothing in the
  request to explain the change — and on a busy install the flip landed
  mid-dashboard-refresh. Both lanes now build the same expression, and both
  read the column's *text* form rather than whatever type `read_json`
  inferred for the batch (JSON's cast domain is narrower than VARCHAR's for
  a fractional string and wider for a number under a BOOLEAN pin, so the
  inferred-type cast made one event's reading depend on what else shared its
  snapshot). Expect **fewer** hot matches on values that never conformed:
  they are now NULL — and therefore *unknown*, not false — from the moment
  they land, exactly as the corpus has always held them.

  Two things about what is **written** change with it, both permanent:
  - the pin ladder scores its candidates through that same guard over that
    same text, so a column of integral doubles beyond 2^53 now pins
    **BIGINT** where it pinned DOUBLE — `1735689600123456710.7` conforms
    as `1735689600123456800`, the integer its own DOUBLE rendering names.
    A pin slot is spent for the life of the install, so this is a durable
    change of both the column's type and the value stored in it;
  - a field pinned **VARCHAR** whose batch `read_json` typed DOUBLE (or
    DECIMAL, or nested) is stored in the `to_json` spelling:
    `100000000000000000000.0` rather than `1e+20`, `1e-7` rather than
    `1e-07`. Under that pin the guard is the identity, so the text form
    *is* the stored value, and a lane that picked its own spelling was a
    lane with its own corpus — the same flip this entry is about, one
    level down.

- **The live tail conforms a value before comparing it (ADR-0011, #63).**
  Under a BIGINT/DOUBLE/BOOLEAN/TIMESTAMP pin, `GET /api/v1/stream`
  compared the value the *sender* wrote while `/api/v1/query` compared the
  value the *catalog stored*, so everything the round-trip guard nulls out
  answered differently on the two paths. A wire `1.5` under a BIGINT pin
  matched `duration>1` on the stream and was unknown to the query; `"abc"`
  matched `duration!=2` in batch (the emitted form carries
  `OR col IS NULL`) and not on the stream; `"TRUE"` under a BOOLEAN pin
  made `NOT flag=true` fire live while `/api/v1/query` returned nothing.
  The stream now reads the conformed value, so **a live tail on a pinned
  field matches exactly what the equivalent query matches** — expect fewer
  stream hits on values that never conformed, and `!=` to start matching
  them. Nothing about `/api/v1/query` changes: the emitted SQL is
  unchanged byte for byte. The same round closed the remaining places the
  in-memory mirror read a text differently from DuckDB's own casts — the
  DECIMAL cast forgiving a scan that whitespace cut short (`"- "` reads
  zero), its exponent path rounding on the leading digit (`5e-8` reads one
  microstep where `0.00000005` reads zero), and `inf ` reading as infinity
  where the cast NULLs it — each of which was a live match the query did
  not have.

- **Custom timestamp fields are conformed zone-aware, in UTC (ADR-0011, #63).**
  A TIMESTAMP-pinned custom field whose value carried an offset was conformed
  with `TRY_CAST(text AS TIMESTAMP)`, a **wall-clock** parse that ignores the
  offset: `2026-01-15T09:00:00+05:30` stored `09:00`, while the same value
  arriving in a batch DuckDB happened to infer as TIMESTAMP stored `03:30`.
  Every conform — compaction, the boot conformance pass, the hot branch, and
  the live-tail pattern text — now parses through `TIMESTAMPTZ`, so an offset
  is **applied** and a zoneless text reads as UTC; `_time`-style globs on such
  a field follow (`/T03:30/` where `/T09:00/` used to match). Because that
  parse consults the session zone, every DuckDB connection that conforms,
  scores the pin ladder, or reads a hot snapshot now sets `TimeZone='UTC'` —
  previously the bundled ICU build defaulted to the **host** zone, so a stored
  instant could depend on `/etc/localtime`. **Data compacted before this
  release keeps its wall-clock values**; they are not rewritten, and mixed
  history is possible for a field that received offset-bearing values (the
  ADR-0011 repin rewrite is the mechanism that would restate them). The
  envelope's own `_time`/`_ingested` are unaffected — ingest canonicalizes
  them to RFC 3339 UTC before storage (ADR-0008).

- **VARCHAR-pinned numeric comparison is exact for every 64-bit integer
  (ADR-0011, #63).** The numeric arm of `=`/`!=`/IN and the ordered rungs
  compared through DOUBLE, which is blind above 2^53 — and blind identically
  in both engines, so live/batch parity looked perfect while both answers
  were wrong. `id=1737000000123456789` returned **three** distinct stored
  ids, and `id!=9007199254740993` silently suppressed the genuinely different
  `9007199254740992`; snowflake ids and nanosecond epochs sit in VARCHAR
  fields in exactly that shape. Both rungs now read the column *and* the
  literal through the same `DECIMAL(38,6)` cast (the literal binds as its own
  text, so it never round-trips through `f64`), which is exact for every
  `i64` and out to 10^32 and is also the space the conform guard compares in.
  Two deliberate narrowings come with it, and both narrow what *matches*, not
  what agrees: a stored `"nan"`/`"inf"` no longer sorts above every number
  (`dur>1` used to return it, and now matches nothing — no reading is
  *unknown*, never a false match), and stored or queried magnitudes at or
  above 10^32 likewise have no reading. Fractions quantize at 10^-6, rounded
  half away from zero, so two VARCHAR-stored values a nanosecond apart now
  compare equal.

### Added
- **fleet-ui grows `Atmosphere`, a WebGL mesh-gradient backdrop (ADR-0012; tracking issue jakub/coastwatch#308).** A decorative, theme-reactive shader layer over a vendored, committed `@paper-design/shaders` 0.0.79 ESM bundle (142 KB against the 500 KB cap, Apache-2.0 attribution stamped into the artifact itself, drift-gated in CI alongside the trawl-web-ui bundles) — mounted under a consumer's login card by one component and one feature flag: the bundle is a compile-time wasm-bindgen snippet, so consumers inherit it through the crate dependency with no build wiring of their own, but *linking* that snippet is also what plants its 142 KB in a dist, so the backdrop sits behind the default-off `atmosphere` cargo feature and only a consumer that mounts it pays the bytes (`features = ["atmosphere"]` on the dep, or `data-cargo-features` on Trunk's `rel="rust"` link). Every tunable lives in `atmosphere::palette`, the single Rust knobs site, with the two `--accent` anchors machine-pinned against `fleet-ui.css`; theme flips re-color the mounted mesh in place (never a remount), `prefers-reduced-motion` freezes it to a static frame, and WebGL-less or context-lost sessions silently keep the `var(--bg)` CSS floor. `.login-shell` no longer declares a background (zero visual delta today — body's floor propagates to the viewport canvas — and the composability the backdrop needs; coastwatch absorbs it on its next `TRAWL_REV` bump). The fleet-ui workbench's `/login` route composes the backdrop live (opting into the feature via `data-cargo-features`), and its trunk build joins CI as the only wasm-target build of this code. trawl-web-ui does not mount it in this slice and does not enable the feature — CI asserts both sides: the workbench dist carries the snippet, the SPA dist does not.
- **Self-telemetry gets a bounded retry queue and first-class loss metrics (#56).** A failed telemetry WAL flush now *retains* its batch on a FIFO retry queue instead of dropping it, and drains oldest-first once the volume recovers — coalescing consecutive queued batches into WAL writes of at most 4 MiB (one file, one `batch_id`, one published batch) so a long outage recovers in a handful of fsynced writes instead of one per flush tick, while nothing merges until a write has actually succeeded — with the WAL write (and both fsync barriers) moved off the async executor onto Tokio's blocking pool. The durability-before-visibility invariant is unchanged and now exactly-once: a batch reaches the hot buffer and SSE strictly after its WAL write succeeds. Retained memory is capped by the new `[ingest] telemetry_buffer_max_bytes` (default 16 MiB, an estimated charge like `hot_buffer_max_bytes`) — one budget over the active buffer, the retry queue *and* the batch in flight through a write, enforced as events arrive so a wedged write cannot let the active buffer grow unbounded; over budget the *oldest* queued batches are shed first and then the incoming event itself, all with exact accounting, and the previously-silent pre-init bootstrap cap now counts its drops too. New Prometheus series — `trawl_telemetry_wal_write_failures_total`, `trawl_telemetry_{events,bytes}_dropped_total{reason="preinit_cap"|"buffer_cap"}`, and the `trawl_telemetry_buffer_{events,bytes}` depth gauges — stay scrapeable precisely while self-ingestion is unavailable; the searchable `telemetry_dropped` recovery event now carries event counts and per-reason totals. Graceful shutdown attempts a final flush under a 5-second budget so an unhealthy volume cannot hang the daemon.
- **Catalog schema surface: per-service schema types and read commands (ADR-0009 slice 3, #51).** The field catalog becomes the type authority behind the *existing* schema surfaces and gains read commands for schema health. `GET /api/v1/schema` is now a postgres `SELECT` over the catalog instead of a corpus-wide parquet `DESCRIBE` (the `schema_cache_ttl_secs` TTL still fronts the unscoped column set — a newly pinned field can take that long to appear — but `cached` refers to the filesystem corpus-facts walk, and a `?service=` request is always served fresh; the endpoint therefore requires the app-state database — down means 503, like history/saved) and gains `?service=` (only fields that service has carried) and `last_seen` windowing against `[retention] max_age_days` (`?all=true` lifts it; never-observed pins — e.g. the envelope — always show), so autocomplete stops offering fields whose data aged out. `GET /api/v1/schema/services` keeps its footer-true stats (nulls, min/max, bytes, daily volumes) but takes its types from the in-process pin cache — the background refresh no longer needs DuckDB at all, and a physically-present column with no pin (foreign or boot-skipped parquet) reports the sentinel type `UNPINNED`; the response shape consumed by the TUI and web UI is byte-identical, though type *spellings* can change (e.g. a physical `INTEGER` column now reports its pin, `BIGINT`). Three new `schema_read`-gated routes — `/api/v1/schema/fields` (pinned fields with per-service/conflict aggregates and catalog fill), `/api/v1/schema/field?name=` (one field's pin, a bounded page of its per-service observations, and its conflict evidence; the name is a query parameter since a catalog key may contain `/`, ASCII-folded before lookup, 404 when unpinned — observations are keyset-paged behind `limit` (default 100, hard max 1000) and an opaque `services_cursor`/`after` pair, because service names are client-chosen and their observation rows are never removed, so one field's history can grow without spending a pin slot), and `/api/v1/schema/conflicts` (recent conflicts, `since_secs`-windowed) — back the new CLI commands `trawl schema fields [--service|--last|--limit|--data]`, `trawl schema field <name> [--limit|--after]`, and `trawl schema conflicts [--field|--service|--last|--limit]`. `trawl schema fields --data <glob>` is the embedded path: a plain `DESCRIBE` over local parquet with no server or postgres (names + physical types only). Both schema endpoints and the CLI list columns in query-result display order (envelope first, metadata last) instead of alphabetically. Because `?service=` and the window are only as good as the observations behind them, the boot conformance pass now **backfills** `field_services` from the corpus it adopts — idempotently, stamped from each file's partition hour rather than from boot time — so a service whose data all predates the upgrade still answers `?service=` and still ages out on schedule; a corpus older than `max_age_days` that retention has not pruned needs `?all=true` to list its fields.
- **Fleet-owned local development controller (`fleet-dev`, ADR-0010, #54).** A new Rust controller replaces Trawl's hand-written launcher with one convention-heavy path for localhost/Docker and CNPG/Tailscale development: strict versioned manifests and machine profiles, redacted pure plans, non-mutating doctor checks, demand-driven 1Password service-account resolution, private per-process `mprocs` configuration, a dedicated persistent loopback-only PG18/pgvector provider, application-owned migrations, provider-scoped Fleet developer credentials, shared development SSO settings, explicit conflict-safe Tailscale Serve setup, signal/exit propagation, and single-stack locking. `bin/dev` remains as a thin legacy flag translator; Coastwatch adoption follows in its companion issue.

### Changed
- **Backend errors are visible under the shipped default log filter (#56).** The default filter becomes an explicit cross-packaging contract — `trawl_server=info,trawld=info,fleet_auth=info,auth.backend=info,storage.backend=info,preauth.transport=info` in the code fallback, Helm `logLevel`, Debian environment example, and docs. The old `trawl_server=info` default silently dropped the deliberately-targeted `auth.backend`/`storage.backend` alarm events, all `fleet_auth` middleware events, and everything `main.rs` itself emits (its target is `trawld`); the Debian example even suggested `trawld=info` alone, which would have filtered out essentially all daemon events. On upgrade, expect these previously-invisible events to appear in the log (startup banner, backend errors, fleet-auth warnings); log volume rises slightly, and persisted telemetry volume rises only for the targets trawl itself emits under (the auth targets are log-only, see below). An existing custom `RUST_LOG` remains authoritative; an *invalid* one now installs the default and emits one `config_warning` with the parse error (never the raw env value), and a pre-tracing config-file failure prints the resolved config path to stderr.
- **Unmetered rejections are logged but never persisted (#56).** The `fleet_auth`, `auth.backend`, `preauth.transport` and `trawl_server::policy::unmetered` targets are excluded from `service=trawld` telemetry regardless of `RUST_LOG` — they still print on stdout (and to `log_file` when telemetry is disabled). fleet-auth's bearer middleware necessarily runs *before* per-key rate limiting, so with the widened default filter every unauthenticated request would otherwise have written one durable ~400-byte record into the WAL and the corpus, letting an unauthenticated flood grow trawl's own storage (and, at scale, evict real partitions under retention); the same held for `auth.backend` errors during a keystore outage. The accept loop's `tls_handshake_failed` / `connection_error` diagnostics move onto the new `preauth.transport` target for the same reason and a cheaper one — a bare TCP connect-and-close provokes a handshake warning with no request, no TLS session and no key involved. The policy layer's grantless 403 joins them on the new `trawl_server::policy::unmetered` sub-target: `require_trawl_grant` is mounted outside the limiter by design (a grantless key never spends a bucket to be told no), and the shared fleet keystore is meant to hold keys with zero trawl permissions, so its holder could otherwise hammer any authenticated route into one durable record per request. Failed authentication, failed handshakes and grantless 403s are now log-pipeline signals: the corpus still carries everything trawld emits behind the limiter, the `storage.backend` alarm target, and the catalog/health events an outage produces. Alerting does not move to the log pipeline with them — the new `trawl_auth_failures_total{reason}` counter (`unauthorized`, `backend_unavailable`, `no_trawl_grant`, plus defensive `forbidden`/`internal`) counts every rejection on `/metrics`, from trawl's own policy layer outside the bearer shell. The label set is closed and carries no key, name or path, so unlike a durable record it cannot be amplified by a flood — credential stuffing, token brute force and a revoked key still in use stay alarmable.
- **Default query lifecycle telemetry no longer contains raw DSL (#56).** `query_start`/`query_complete`/`query_timeout`/`query_failed`, `export_start`/`export_complete`, and `stream_start` now share a `query_id` (allocated before the first event of each) and carry `query_len` plus the existing actor/roles/outcome/timing/pagination/row-count fields — the query text itself is gone from the default `service=trawld` corpus and its 90-day retention, for exports and live tails as well as `/query`. Failures carry a stable `error_class` (`parse`, `emit`, `database`, `timeout`, `store`, …) *instead of* an error message: parser and emitter messages quote the user's own tokens and invalid format strings, and the raw database error embeds the generated SQL, so neither belongs in retained telemetry. Operators who relied on any of it must use authenticated query history, the query debug log, or the DEBUG-only `query_text` / `query_error_text` events (`RUST_LOG=trawl_server=debug`).
- **Query-debug and TUI log files are owner-only and bounded (#56).** Both open `0600` on Unix — a pre-existing looser file is tightened at open, and because POSIX `chmod` is the owner's privilege, a foreign-owned log that cannot be re-moded refuses the open *only* when it is genuinely group/other-accessible (an already-owner-only one warns and keeps serving, so an opt-in debug file never costs trawld its boot) — and the query debug log gains `server.query_log_max_bytes` (default 100 MiB, `0` disables) with single-file rollover to `<path>.1`, plus a startup warning spelling out that it records raw query text, SQL parameter values, and result samples. Docs now state the telemetry boundary honestly: internal telemetry covers `trawld` only, `trawl-web`/CLI logs are stdout-only, the fleet key audit is a coalescing 30-second poll rather than a transactional ledger, and there is no OTLP export.
- **The last read-time type reconciler is deleted (ADR-0009 slice 3, #51).** `describe_schema_columns_coerced` and its per-file reconciliation helpers are gone; `Executor::describe_schema` shrinks to a single `DESCRIBE ... union_by_name=true` retained for embedded mode only, with errors propagated. User-visible only for embedded mode over *foreign* parquet: a query across irreconcilably drifted columns now errors loudly where it previously coerced the column to `VARCHAR` (the describe itself reads no rows and reports the footer union raw). Trawl-written files can never hit this — write-time catalog conformance guarantees it.
- **BREAKING — the field catalog: write-time type conformance replaces every read-time repair (ADR-0009 slice 2, #50).** Every custom field's type is now *pinned* in a postgres-backed catalog the first time a typed batch carries it (all-null batches defer; nested objects/arrays are stringified at ingest, pin `VARCHAR`, and stay reachable via `json_extract_string` — see the DSL reference). Compaction makes pins durable *before* any parquet carrying them is published and conforms each batch to them: a value disagreeing with its pin becomes `NULL` (recorded in `field_conflicts`, counted by the new `trawl_catalog_conflicts_total` / `trawl_catalog_rows_nulled_total` metrics, original findable in `_raw`), so **every parquet file trawl writes conforms to the catalog and unions can never type-conflict**. A typed cast counts as a success — in the pin ladder and at conform time — only when the value survives the round trip unchanged: bare `TRY_CAST` *rounds* (`1.5` → `2`), so without the guard a fractional batch would pin `BIGINT` and be silently rounded on every write with no conflict recorded; now such a batch pins `DOUBLE`, a rounded value is written as `NULL` and tallied as a conflict, and a ≥90%-boolean batch pins `BOOLEAN` (previously unreachable, since `TRY_CAST(true AS BIGINT)` "succeeded" as `1`). Because the pin is keyed on the field *name*, ingest now bounds names to **255 bytes**: a longer name could never be a postgres btree key, so it could never be pinned — and an unpinnable column would stall that service's compaction indefinitely (WAL retained and re-failed every tick) rather than costing one field. An over-long field is dropped from the event with the new `field.name_too_long` repair code, its name and value still recoverable from `_raw`; ingest now also **ASCII-lowercases every field name** — `DuckDB` identifiers are ASCII case-insensitive, so `Dur` and `dur` name the *same* column, and without folding the two spellings pinned independently and made every spanning query fail with a hard Conversion error. Mixed-case field names arrive lowercased, recorded per event in `_repairs` as the new `field.name_case_folded` code (only when folding changed something); two keys in one event that collide after folding keep ONE value — the exact-lowercase spelling's when present, else the ASCII-lexicographically-first variant's — with the loser dropped as the new `field.name_case_collision` code, both originals findable in `_raw`. A case-variant of an envelope column is simply consumed as that column (`_Time` is the `_time` wire input) unless the exact spelling is present too, in which case the exact one always wins. The boot conformance pass folds the same way: pin votes group by folded name and rewrites rename standing mixed-case columns to the folded form; a name arriving at the catalog from anywhere else (foreign parquet met by the boot pass) is skipped with a `catalog_field_name_unstorable` warning instead of failing the batch. Field and service names being client-chosen, the catalog is bounded on every axis a sender controls: at most **10,000** pins ever exist (the surplus stays unpinned and unstored — `catalog_pin_cap_reached`, counted by `trawl_catalog_pins_rejected_total`), and because a pin slot is spent permanently, compaction may claim at most **half the free slots per batch** — so no single ingest request can take the whole catalog and leave every later field on the install unstored; the fill level is exported as `trawl_catalog_pinned_fields` against `trawl_catalog_pin_capacity` so an operator alerts on the slope rather than on the cliff (the boot conformance pass is exempt from the ration: its proposals are columns already on disk), and `field_conflicts` keeps a rolling window of the **100 newest rows per field**, trimmed in the transaction that appends, so a sender that disagrees with a pin on every tick costs a fixed amount of postgres rather than a growing one (the `trawl_catalog_conflicts_total` / `trawl_catalog_rows_nulled_total` counters remain exhaustive). On that invariant the entire read-time reconciliation net is deleted — the hot/cold VARCHAR-coercion retry, the compaction merge cast fallback, and the rollup cast fallback. The hot buffer participates too: each query snapshot carries the catalog pins intersected with its observed keys, and the emitter conforms the hot branch of the union so an uncompacted conflicting event degrades to `NULL` for that field while the full cold history stays visible. Operator notes: (1) **foreign or nonconformant parquet now errors loudly** — a type conflict at query, export, merge, or rollup time returns an error (`catalog_invariant_violation` in compaction logs, inputs retained and retried) instead of silently degrading to a hot-only 200 or a stringified rewrite; drop-in parquet must conform to the catalog, and a data-root restore against a different catalog forces a boot re-scan via the `data/CATALOG` identity marker. (2) **The first boot after this upgrade scans the whole corpus** — and rewrites any file that disagrees with the most-rows-wins pins (staged `.tmp` + atomic renames; restartable; `scheduled/` excluded) — so expect one longer startup proportional to corpus size. (3) **An ingest-node boot now requires reachable postgres**: the conformance pass is fatal on failure, because an unproven corpus must not serve queries once the read-time safety nets are gone — but a *single* unreadable or foreign `.parquet` is not such a failure: it is sniffed, skipped with a `catalog_conform_skip` warning and a new `trawl_catalog_conform_skipped_total` counter, left untouched, and the pass withholds the `data/CATALOG` marker so the next boot retries it. (4) **The pass only ever rewrites files it could have written itself.** The rewrite is in place and lossy, so ownership is decided from the *path* before the file is opened: only `{env}/{date}/{HH}/{service}.parquet` (or the daily rollup `{env}/{date}/{service}.parquet`), with every component passing the same injective name predicates ingest enforces, is adopted. A readable parquet parked anywhere else under the data root — an operator's own export subtree, a reserved env name — is skipped exactly like an unreadable one: byte-identical, never scanned, never voting on a pin, and re-examined on every boot until it is moved out of the tree.
- **BREAKING — the event schema cutover: declared envelope, env path dimension, OTel severity, `_raw`/`_repairs`, and the `data/EPOCH` reset (ADR-0009 slice 1, #52).** trawl now declares and enforces a ten-field event envelope at ingest — `_time`, `_ingested`, `_raw`, `_repairs`, `env`, `service`, `host`, `severity`, `severity_text`, `message` (the `_` prefix marks metadata about the record's handling). **Legacy data is dropped**: on first boot the old `data/` root (parquet and WAL together, plus an external `wal_dir`) is set aside as `data.pre-schema-v2/` via a restartable `data/EPOCH` decision table — trawl never deletes the set-aside directory (it needs a writable parent; boot refuses with instructions otherwise) and suppresses disk-pressure retention while it exists — the set-aside is invisible to the partition scan yet owns the same filesystem, so deleting fresh partitions could never reclaim it (a pressured tick logs `retention_disk_pressure_suppressed`; age-based retention is unaffected) — and a root with no marker next to an existing set-aside refuses to start rather than guess. The rename fires only on evidence trawl wrote the directory (a `wal/` subdir, a `YYYY-MM-DD` partition dir or a parquet file) and only when `ingest.enabled` is true: a marker-less directory holding no trawl data (a pre-created empty root, a fresh mount, a mistyped `[data] path`) is adopted in place with nothing moved, and a query-only node pointed at a shared parquet archive is left entirely untouched. Scheduled-report parquet under `scheduled/` is the one subtree that does *not* move with the root: it holds materialized query results rather than epoch-1 events, and each is referenced by relative path from a live postgres `report_runs` row the cutover deliberately leaves untouched, so it rides across into the fresh root instead of dangling every one of those rows behind a silent `result: null` (one rename, retried on every boot that sees a set-aside, so a crash mid-cutover still ends with the rows resolving). The wire contract keeps existing senders working: `timestamp`/`@timestamp` are consumed as the `_time` input (the ADR-0008 repair grammar is unchanged) and `level` feeds severity derivation (integer `severity` 1-24 wins, then `severity_text`, then `level`; syslog numerics 0-7 are accepted and **inverted** onto the OTel ladder) — but none of the aliases are stored, and senders providing neither alias nor `_time` now get arrival time with the `time.from_ingest` repair code. Every server-side substitution is recorded: `_repairs` carries codes from a closed enum (`host.from_peer`, `env.defaulted`, `time.from_ingest`, `time.out_of_range`, `severity.unmapped`, `field.truncated`, `meta.stripped`) and the new `trawl_ingest_repairs_total{code, service}` counter **replaces** `trawl_ingest_events_repaired_total`; `timestamp_invalid` is retired (the original is findable in `_raw`, which also joins bare-word search alongside `message` — making a bare term **whole-event search**: where the server filled `_raw` with the event's serialization, a term matches any field's *value* and any field *name*, and negation excludes on the same basis, so use a field filter like `message=/debug/` to confine a match to one column). Storage gains the `env` path dimension — `data/{env}/{date}/{HH}/{service}.parquet`, WAL under `wal/{env}/` — governed by the new `[ingest] envs` allowlist (`default_env` fills missing envs; unlisted envs hard-reject; validated at boot) with `trusted_relays` CIDRs rejecting host-less events from relays instead of stamping the relay's address. Path encoding is injective by validation: the service filename sanitizer is deleted (so `api.v2` and `api_v2` are distinct files and pruning is exact), **spaces in service names now reject**, as do `.`/`..`/dot-leading names. DSL: `level` compiles to numeric severity-band predicates (`level=error` → `severity BETWEEN 17 AND 20`, `level>=warn` → `severity >= 13`; `notice` falls inside the INFO band); a free-text/glob/regex `level` value is now a **query error** naming the valid tokens, and `level` in projections/`stats by`/`sort` surfaces a column-not-found (use `severity`/`severity_text` — documented limitation). `timestamp`/`@timestamp` become query aliases *to* the physical `_time` column. The shipped Vector configs emit the new schema natively and the Vector integration guide now documents the full field mapping.

- **BREAKING — roles-as-data RBAC cutover (ADR-0006 slice 1, #44).** fleet-auth's static `(key, app, role)` grant model is replaced by data-defined roles: `roles` / `role_permissions` / `key_roles` tables plus a warn-only `app_permissions` vocabulary registry. A role is a named, cross-app bundle of permission strings with an optional `rate_rpm` ceiling; keys hold any number of roles and effective permissions are the union — tiers can now be reshaped with `fleet-admin roles` without a deploy. The migration converts every legacy grant in place to a role named `<app>-<role>` (`trawl-admin`, `coastwatch-analyst`, …) carrying the permission set the owning app hardcoded (trawl's minus the dead `key_manage`), links keys via `key_roles`, seeds trawl's vocabulary, and **drops `api_key_role_assignment` irreversibly** — existing tokens keep working with unchanged capability, but the migration is the runtime cut point for every app on the shared fleet database (coastwatch's companion arc must ship in the same window; see the cutover runbook). Wire changes: `/whoami` replaces `assignments` with `roles: [names]` (permissions unchanged: recognized trawl permissions in canonical order); trawl-web's `/login` and `/me` replace `role: String` with `roles` + `permissions` and gate on "≥1 resolved trawl permission"; active-query/log `role` labels become the comma-joined role-name list. `fleet-admin` grows `roles create/list/show/add-perm/remove-perm/set-rate/delete` (`set-rate <NAME> --rate-rpm N | --default` re-tiers or clears a role's ceiling in place, keeping its bundle and key assignments; delete refuses while keys hold the role unless `--force`; unknown permissions warn on stderr but persist) and `keys create --role` / `assign-role` / `unassign-role` replace `--grant` / `grant` / `revoke-grant`. Rate limiting: a role `rate_rpm` (max across the key's roles) **overrides** the route-class defaults, applied independently per class. The SPA's admin gating re-keys from the role name onto the `server_manage` permission.
- **BREAKING — rate limiting is per API key, not per role (ADR-0006 slice 0, #43).** Every key now gets an independent token bucket keyed by its keystore id, so one noisy key can no longer starve others; the `[server.rate_limit]` per-role knobs (`admin`, `analyst`, `reader`, `ingest`) are replaced by two route-class knobs: `default_rpm` for the interactive API routes (default 100 — the loosest legacy *interactive* ceiling, so an upgrade never widens a reader key to the shipper-sized budget) and `ingest_rpm` for `/api/v1/ingest` (default 1000 — the legacy ingest ceiling, unchanged); `0` disables a class. `ingest_rpm` is earned by the `ingest` permission, not by the route: a key without it stays on its `default_rpm` bucket when it posts to `/api/v1/ingest`, so the shipper-sized ceiling never widens an unprivileged key on the heaviest endpoint. A config still carrying a per-role key fails validation at boot with a message naming the migration, and the helm chart fails at render if a values file still sets `rateLimit.admin`/`analyst`/`reader`/`ingest` (`rateLimit.defaultRpm` + `rateLimit.ingestRpm` replace them) — never a silent ignore of tuned quotas. 429 semantics and response shape are unchanged. Role-differentiated class-of-service is deliberately deferred to slice 1, where it returns as a `rate_rpm` role attribute; the `rate_limit_exceeded` log event now carries `key_id` instead of `role`.
- **Web UI reskin: Mira Blue design language (ADR-0007, #47).** fleet-ui and the trawl web UI move onto Basecoat Mira's geometry and neutral zero-chroma OKLCH surfaces with fleet's blue accent retained — a pure CSS/HTML pass (no component markup changes). Buttons drop to weight 500 with transparent borders, `color-mix` tint hovers, and a 1px translate press; destructive buttons become tinted (red text on a red wash) instead of solid; inputs and the DSL editor get translucent line-tint fills; focus is a 2px solid accent-tinted ring; radii are tokenized (`--radius-panel/ctl/sm/...`); button text reads the new `--on-accent` token, and dark mode keeps Mira's inversion (near-black text on the light-blue accent). Fonts swap from Open Sans / Fira Code to **Geist / Geist Mono** (still Google Fonts, self-hosting deferred), and `color-scheme` + `scrollbar-color` are declared so Firefox native widgets follow the theme. The `css_chrome_parity` golden fixture is re-captured at this baseline. coastwatch is unaffected until its next `TRAWL_REV` bump, where it runs its own adoption pass.
- **Web UI assets are precompressed and content-negotiated.** Release builds emit deterministic Brotli/gzip sidecars, enforce whole-SPA wire budgets (2.5 MiB Brotli, 4 MiB gzip), and bake every representation into `trawl-web`. Embedded and `TRAWL_WEB_SPA_DIR` delivery now negotiate `Accept-Encoding`, preserve the original MIME type, emit representation-specific ETags with conditional 304 support, and retain immutable caching for Trunk-hashed assets. `bin/dev --release-spa` makes remote/Tailscale iteration use optimized Wasm instead of the roughly 145 MiB debug module.

### Fixed
- **A malformed ingest timestamp can no longer wedge compaction or silently drop a service's parquet history (ADR-0008, #49).** Previously an event whose `timestamp` was present but not castable to `TIMESTAMP` (`"not-a-date"`, an out-of-range date, a nested object, a bare number) was accepted with HTTP 200 and then poisoned everything downstream: the batch hard-CAST failed the whole compaction chunk forever (WAL never drained, never quarantined, never reclaimed), and every hot+cold query on that service was misclassified as a schema conflict and silently degraded to hot-only — dropping the entire parquet history from results while still returning 200. Now: (1) ingest canonicalizes valid timestamps (RFC 3339; ISO 8601 basic offsets like `+0530`/`+02` as Java and Go encoders emit; offset-less date-times read as UTC; `T` or space separator, `YYYY-MM-DD` or `YYYY/MM/DD` dates, optional seconds and fraction; or a bare date at midnight UTC — surrounding whitespace trimmed) to RFC 3339 UTC microseconds, and substitutes malformed ones with the arrival time, preserving the original verbatim in a `timestamp_invalid` field (new `trawl_ingest_events_repaired_total` counter + `ingest_repairs` warn); (2) compaction never hard-CASTs the partition key — `TRY_CAST` falls back per-row to the ingest instant encoded in the row's own WAL filename (this drains WAL already wedged on disk from before the fix, no operator step), then to the compaction instant, so no parquet row ever carries a NULL timestamp; (3) the hot/cold union `TRY_CAST`s the hot side's timestamp; (4) error classification keys off DuckDB's error-class token instead of substring-sniffing, and the query outcome policy now returns an **error** whenever a database failure could hide existing cold data — including a read that matched no files while cold parquet exists on disk (a transient race, surfaced as a retryable 503 rather than an empty result) — a cold-data drop is never a silent HTTP 200 (hot-only fallback remains for genuine cold starts and missing-column user errors; parquet export routes through the same gate *and* through the same repairs the gate assumes ran first, so an export never hard-errors on a corpus a query answers). A columnless result is not accepted as proof of a cold start either: DuckDB rejects a whole list source when a *single* element matches no file, which happens routinely for a sparse-traffic service whose hour directories exist without its parquet in them, so the query is retried over just the elements that do match — the cold rows that exist are returned instead of being dropped in favour of hot-only. Note: rows drained from pre-fix WAL carry ingest time, not event time, and their original bad values are not preserved — only post-fix ingest preserves originals. The per-row WAL-filename provenance in (2) travels as a reserved `_trawl_wal_file` column, so that field name is now trawl's: ingest silently drops it from incoming events (the rest of the event is accepted unchanged), and WAL already carrying it still compacts, losslessly, under a renamed provenance column.
- **Web UI: the DSL editor's line-number gutter follows the theme.** CodeMirror ships an unconditional light base theme for the gutter rail, the active-line band, and the caret, and nothing installs a CM dark theme — so in dark mode the rail rendered as a near-white slab down the left of an otherwise near-black editor. `.cm-gutters` / `.cm-activeLine` / `.cm-content` are now tokenized (`--panel`, `--line`, `--ink-4`, `--fill`, `--ink`) like the rest of the editor frame — the rail repaints the frame's own resting surface, opaquely, since it is `position: sticky` over content that scrolls horizontally beneath it. Pre-existing since the editor landed, not a Mira Blue regression (#47).

## [0.4.0] - 2026-07-24

### Added
- **Live admin stats in the web UI footer**, fed by a new SSE endpoint. `GET /api/v1/dashboard/stream` (server) emits a cached dashboard snapshot as named `stats` events every 2s, `ServerManage`-gated and bounded by its own semaphore so footer connections never consume user stream slots; `trawl-web` proxies it as a first-class SSE pass-through. The web UI replaces the three stubbed footer groups with live hot-buffer, WAL-backlog, active-query, and uptime figures, opened only after `/me` resolves as admin (non-admins never issue the request) and cleared if the stream closes so the numbers never freeze stale.
- **Web UI: sortable table headers** across the schema and nets tables (click to sort, click again to flip), and a **compact/comfortable density toggle** in the statusbar that rescales facets, chips, histogram axes, tables, and the editor together via the fleet-ui dense type sub-scale.
- **Fleet SSO opt-in for the web UI**: a `[web] shared_domain` config knob (env-mirrored, documented for reverse proxies) sets the `fleet_session` cookie `Domain=` so a single sign-in is shared across fleet apps under a common parent domain. Surfaced in both channels — a commented `shared_domain` in the deb `trawld.toml` and `web.sharedDomain` in the helm chart — with an SSO provisioning runbook (ADR-0004 slice 2).
- `fleet-admin keys revoke-grant <prefix> <app>` and `fleet-admin keys retype <prefix> <kind>`, completing key-lifecycle parity with trawl-admin ahead of the trawld keystore cutover (ADR-0004 slice 0). `revoke-grant` follows the `keys revoke` confirmation convention (`--yes`/`-y`, `[y/N]` prompt, non-TTY refusal without `--yes`) and accepts `app` or `app:role` (role half ignored); a missing grant errors with `GrantNotFound` rather than silently succeeding. `retype` flips a key between `human` and `service` and refuses revoked keys (#35).

### Changed
- **BREAKING — app stores move to a dedicated `trawl` postgres database; `trawl-auth` crate deleted (ADR-0004 slice 3, #41, closes #12).** Query history, saved queries, schedules, and report runs leave the transitional sqlite file for a postgres database that trawld owns outright: it auto-migrates the schema at boot (sole writer) and holds a session advisory lock for its lifetime, so a second trawld against the same database fails startup instead of racing. New `[storage] database_url` config (`TRAWL_DATABASE_URL` env takes precedence, NO fallback to the auth URL); `[auth]`'s env override is renamed to `FLEET_DATABASE_URL` — the bare `DATABASE_URL` is no longer read by trawld (it belongs to fleet-admin and the sqlx test harness) — and `db_path` is rejected at config validation with a message naming this migration. No sqlite importer (hard-cutover doctrine): recreate saved queries and schedules per the cutover runbook. Wire shapes are unchanged, but duplicate-name and schedule-exists conflicts now return **409** (previously 400) per the store error table, the saved-query list is served by one bulk-join statement instead of `1 + 3n` lookups, and `/health` gains a timeout-bounded `storage_db` check (degraded ⇒ HTTP 200). Concurrency guards are real cross-connection guarantees now (partial unique index for the one-running rule, transactional `max_runs` claims, orphaned result files cleaned up when a run is deleted mid-flight), and a background task now detects loss of the sole-writer advisory lock (pg restart, idle-cull, `pg_terminate_backend`) and hard-exits rather than letting a second writer start split-brain. Helm requires `storage.database.existingSecret` (injected as `TRAWL_DATABASE_URL`) and injects the auth Secret as `FLEET_DATABASE_URL` (the Secret KEY name is unchanged); the deb's `trawld.toml` gains `[storage]`. Internally the workspace returns to the umbrella `sqlx` crate with `sqlx::migrate!()` and `#[sqlx::test]` — the rusqlite `links` collision, the hand-built migrator, and the PgFixture/`pg_test!`/`FLEET_TESTS_REQUIRED` test machinery are all gone (#12).
- **BREAKING — fleet-auth postgres keystore cutover (ADR-0004 slice 1, #36).** trawld now verifies API keys against fleet-auth's external postgres keystore and **will not start until that database is reachable and migrated** (`fleet-admin migrate`): `apt install` alone no longer yields a working server. `[auth]` gains `database_url` (the `DATABASE_URL` env var takes precedence); `auth_cache_ttl_secs` is gone (revocation is now checked per-request in postgres — no TTL window). This is a hard cutover with no data migration: every existing `flt_` token stops working and must be re-minted with `fleet-admin` (admin, CLI, and vector ingest keys), and schedules must be recreated — follow the cutover runbook in the docs. The legacy sqlite `auth.db` is quarantined in place (postgres and sqlite key ids are unrelated sequences; reusing the file would leak one principal's history/saved queries/auto-executing schedules to another) — trawld refuses to start while `db_path` still points at a file named `auth.db`; repoint it at a fresh `store.db`. Keys from other fleet apps now receive an opaque 403 on every authenticated route (previously `/whoami` leaked cross-app assignments to them). `trawl-admin keys` subcommands are removed (`tls` remains); key management lives in `fleet-admin`, which now ships in the docker image and the .deb. The helm chart requires `auth.database.existingSecret` (the DSN is injected as `DATABASE_URL` from a Secret, never rendered into the ConfigMap) and its init container now only runs `fleet-admin migrate` — per-install key auto-minting is gone.
- **BREAKING — `trawl-web` sessions move onto the shared fleet-auth `fleet_session` SSO cookie (ADR-0004 slice 2, #40).** The 444-line hand-rolled `session.rs` is deleted; the cookie's crypto (XChaCha20-Poly1305), builders, and origin validation now come from fleet-auth's `session` feature. The cookie is renamed to `fleet_session` (`SameSite=Lax`), carries an app-agnostic `{token, name, exp}` payload — the role leaves the cookie and `/me` re-derives it live from upstream `/whoami` each request, so grant changes take effect immediately — and its `Domain=` is driven by the new `[web] shared_domain` knob. Cookie-cleared-on-401 behaviour is now decided only by `/me` against the permission-free `/whoami` (a proxied 401 for a mere permission denial no longer signs the user out fleet-wide). Existing `trawl_session` cookies are invalidated by the rename and users must re-authenticate.
- **Web UI reskin: slate/blue design-workbench pass and shared fleet-ui chrome (ADR-0005, ADR-0002/ADR-0030).** The interface moves off the amber theme onto a cool slate/blue palette (accent `#2a5c8a` light / `#5a9fd4` dark) with a golden-ratio type rhythm, sentence-case labels, and Open Sans / Fira Code fonts. The whole chrome — shell, rail, topbar, modals, drawers, tabs, badges, status dots, sparklines, toasts, pagers, loading states — is rebuilt on the shared `fleet-ui` design system, bringing consistent overlay focus management: modals trap focus and restore it to the opener, drawers capture without trapping, `Escape` closes only the topmost overlay, and the overflow (`⋯`) menu is fully keyboard-navigable. The schema page is now a sortable services table (the card grid is gone). **BREAKING for fleet-ui consumers:** the `--amber*` design tokens and `.amber` class are renamed to `--accent*` / `.accent`.
- **MSRV raised 1.88 → 1.94** (required by sqlx 0.9). Only affects from-source builds — shipped binaries are built with a newer toolchain.

### Fixed
- **The date/time scalars now evaluate on the live-streaming path.** `tonumber`, `tostring`, `date_part`, `date_trunc`, `date_diff`, `strftime`, and `strptime` — previously handled only in the batch (DuckDB) path — are now implemented in the in-memory streaming evaluator too, so a `let`/`eval` using them computes correctly in live tail (SSE) instead of silently returning null/unknown. `EvalValue` gains a `Timestamp` variant (naive datetime, mirroring DuckDB's offset-discarding `AS TIMESTAMP` cast), enabling comparisons like `where timestamp > now()` in live tail without pre-parsing the field. A generative test pins both paths to identical results against real DuckDB (ADR-0001, #22, #24).
- **Streaming-vs-DuckDB scalar parity gaps closed** across the scalar functions, each a silent divergence between `/query` (batch) and live tail (SSE): `length()`/`len()` now counts characters, not UTF-8 bytes; `concat()` skips NULL args and casts the rest to text, matching DuckDB `CONCAT` (not `||`); `substr()` follows DuckDB's 1-based, character-counted, negative-start/length window semantics; float-to-string rendering is bit-for-bit `CAST(DOUBLE AS VARCHAR)`; `abs()`/unary-neg return NULL on `i64::MIN` overflow; `tonumber()` strips digit separators like `TRY_CAST`; invalid `strftime`/`strptime` format literals are now rejected up front in both paths (previously batch errored, streaming nulled); and partial `strptime` formats (year-only `%Y`, year-month `%Y-%m`, bare month-day `%m-%d`, date + incomplete time `%Y-%m-%d %H`) fill from the same `1900-01-01 00:00:00` base in both paths (#22, #24, #25).
- Web UI: results rows now re-render when the sort changes (the tbody was evaluated once at mount); the DSL editor renders the Fira Code stack instead of CodeMirror's injected generic `monospace`; histogram bars render in the accent blue with error segments in red.
- Git hooks are POSIX/Linux-portable (they previously assumed a macOS dev box: bash-only `source`, an unconditional `~/.cargo/env`, and `sysctl hw.ncpu` for the CPU count).

### Security
- **Cross-site logout / CSRF hardening in the session path (fleet-auth + trawl-web).** Login and logout now enforce present-only, strictly same-host `Origin` validation (an absent `Origin` still passes for curl/scripts), and the same guard is applied to every cookie-authed proxy mutation (`POST`/`PUT`/`DELETE` saved, schedule, queries, export) so a compromised or attacker-controlled sibling origin can no longer ride the `SameSite=Lax` shared cookie to forge a request or a fleet-wide sign-out. A shared cookie `Domain=` is explicitly not treated as an origin allowlist. Host derivation falls back to the HTTP/2 `:authority` pseudo-header so h2-terminated deployments are covered.
- **Remote-triggerable panics on the SSE streaming eval path closed.** An ordinary query over attacker-influenced event data could abort the client stream (and storm the log with backtraces): a non-char-boundary byte slice in `strip_offset` (reachable via `* | where timestamp > now()` against a ≥6-byte multi-byte field value), a chrono `Item::Error` panic from an invalid `strftime` format (`%Q`, a dangling `%`), and a multi-byte trailing char in the schedule interval parser are all fixed with checked slicing and up-front validation.
- Docs site: astro 6 → 7.1 + starlight 0.41 clears three XSS advisories and pulls svgo 4.0.2 (removeScripts bypass); the transitional `esbuild` override is retired (astro 7 resolves the patched 0.28.1 itself). `crossbeam-epoch` 0.9.18 → 0.9.20 (RUSTSEC-2026-0204).
- Packaging: the fleet-auth Postgres DSN conffiles (`/etc/trawl/trawld.toml`, `/etc/default/trawld`) install `0640 root:trawl` instead of world-readable `0644`, so an unprivileged local user can no longer read the keystore credential.

## [0.3.3] - 2026-06-22

### Added
- Release builds now publish Breakpad symbol files with GNU build-ids, so `trawld` minidumps (from the 0.3.2 crash-dump capture) symbolicate against the Rust frames and the statically linked libduckdb module. Symbols are harvested before the shipped binaries are stripped — keeping the binaries lean — and published to a `symbols/` store on gh-pages for `minidump-stackwalk --symbols-url`. The build asserts the GNU build-id survives stripping on both architectures, failing the release loudly rather than shipping binaries that could never be symbolicated.

### Fixed
- Eliminate a `trawld` crash-loop: the background schema-refresh job called DuckDB's `parquet_metadata()`, which can SIGSEGV on a worker thread (uncatchable) and take the whole daemon down. Per-column schema-browser statistics (null/min/max/compressed size and row counts) are now read directly from parquet footers in safe Rust, so a corrupt or mid-write file is logged and skipped — and retried on the next refresh — instead of crashing the daemon.

### Security
- Override the documentation site's transitive `esbuild` dependency to 0.28.1, clearing GHSA-g7r4-m6w7-qqqr (development-server arbitrary file read; low severity, does not affect the published static site).

## [0.3.2] - 2026-06-21

### Added
- Out-of-process crash-dump (minidump) capture for `trawld`. On a fatal signal (SIGSEGV/SIGABRT/SIGBUS) a re-exec'd monitor process writes a minidump before the process dies, turning an opaque exit-139 into a `.dmp` that `minidump-stackwalk` can symbolicate against the Rust frames and the libduckdb module. New `trawl-crashdump` crate (the one place `unsafe` is allowed in the workspace); opt-in via the Helm `crashDump` block (off by default, requires `CAP_SYS_PTRACE` on the trawld container only). Dumps are written owner-only (`0600`) since they contain raw process memory. Debian/systemd parity is tracked separately.

### Fixed
- Bump DuckDB 1.5.1 → 1.5.4 to pick up JSON/Parquet segfault and out-of-bounds hardening (upstream #21594, #21972, #21635, #23100) on the `read_json`/`read_parquet` paths trawld drives hardest during compaction and hot-buffer queries — the leading suspect for the recurring exit-139 crashes. Pinned a serde recursion-limit regression test so pathologically deep JSON keeps being rejected at ingest rather than reaching the recursive-descent parser.

## [0.3.1] - 2026-06-19

### Fixed
- Quarantine corrupt WAL `.ndjson` files (NUL-filled or truncated torn-write debris from a hard kill) instead of letting one bad file head-of-line-block a service's compaction forever; good files in the same batch still compact, and an all-corrupt batch is counted as data loss rather than retried indefinitely
- Per-file read isolation on WAL compaction: a malformed-but-textual file that slips past the byte sniff is isolated and quarantined rather than wedging the whole batch, so no corruption shape can stall compaction
- fsync WAL writes — data fsync before the rename, parent-directory fsync after — so a hard SIGKILL can no longer leave a full-length but NUL-filled `.ndjson` poison pill; the parent-dir fsync is best-effort so it can't falsely reject an already-durable write

## [0.3.0] - 2026-06-18

### Added
- Documentation site at [trawl.sh](https://trawl.sh) (Starlight/Astro)
- Fuzz testing for parser and emitter (cargo-fuzz + libfuzzer)
- MSRV check (Rust 1.88) in CI
- Configurable syslog channel capacity

### Fixed
- SQL injection in parquet export path (single-quote escaping)
- Gzip decompression bomb — capped at 10x wire size
- Parser panic on multi-byte unicode in error enrichment
- `unreachable!()` in export handler replaced with error return
- Timechart auto-bucket for >30 day ranges (was 1h, now 1d)
- Source list validation handles unspaced comma-separated paths
- Heal hot/cold and cross-file parquet **schema drift**: complex columns (STRUCT/JSON/array) are coerced to VARCHAR at compaction write time and symmetrically in the hot buffer, so a field that is an object in one batch and a plain string in another no longer drops cold/parquet rows at query time or wedges the daily rollup
- Quarantine corrupt/truncated parquet inputs (renamed `.corrupt`) instead of letting one bad file wedge the daily rollup forever; surface the resulting data loss on the compaction error counter
- Daily-rollup accounting hardening: count quarantined inputs even when the merge then hard-errors, count a wedged rollup recovery, and make hourly-file cleanup idempotent so a partially-completed recovery can't loop

### Changed
- Value types (`Value`, `QueryResult`, `SchemaColumn`) moved from trawl-engine to trawl-api
- DuckDB error message strings extracted into named constants
- **BREAKING**: trawl-auth is now a multi-app identity substrate (ADR-0021). API keys carry 0..N `(app, role)` grants in a new `api_key_role_assignment` table instead of a single flat `role` column. Existing v2 databases are auto-migrated; every pre-existing key gets backfilled with a single `("trawl", <old_role>)` grant and `kind = "human"`.
- **BREAKING**: `/api/v1/whoami` response shape now returns `{prefix, name, kind, assignments, permissions}` — the flat `role` field is gone (clients should read `assignments` and find the `"trawl"` entry).
- **BREAKING**: `trawl-admin keys create` replaces `--role <role>` with `--kind <human|service>` plus a repeatable `--grant <app:role>` flag. New subcommands: `keys grant`, `keys revoke-grant`, `keys retype`.

### Security
- Bump dependencies to clear advisories: `tar` 0.4.46 (RUSTSEC PAX desync, build-time), `rand` 0.8.6/0.9.3 (RUSTSEC unsoundness); docs toolchain `astro` 6.4.6 (SSRF/XSS), `vite` 7.3.5 (`fs.deny` bypass), `js-yaml` 4.2.0 (merge-key DoS)

## [0.2.0] - 2026-04-20

### Added
- `trawl-web` (browser session proxy + embedded leptos SPA) now ships by default in both distribution channels
- Debian: `trawl-server` .deb installs the `trawl-web` binary, a sandboxed `trawl-web.service` systemd unit, and `/etc/default/trawl-web`; `postinst` generates a persistent 32-byte session cookie key at `/var/lib/trawl/web.cookie`
- Helm chart: `trawl-web` runs as a sidecar container in the trawld StatefulSet pod (`web.enabled: true` by default) with a chart-managed cookie Secret that survives upgrades via `lookup`
- New `[web]` block in `trawld.toml` (`bind_addr`, `cookie_secret_path`, `session_ttl_secs`, `allow_insecure_cookies`) — shared config for trawld and trawl-web
- Release workflow now builds the SPA with `trunk` + `wasm-bindgen-cli` before `cargo zigbuild` and includes `trawl-web` in release tarballs, .deb packages, and the container image

### Changed
- Helm ingress now targets the trawl-web sidecar by default (`ingress.backend: web`, plain HTTP/8090) instead of trawld's raw HTTPS API. Set `ingress.backend: trawld` to restore the previous behavior for bearer-token API clients
- Container image exposes port 8090 (web UI) alongside 5514 and 1514
- API clients (CLI, `trawl-client`, vector log shippers) continue to talk to trawld on 5514 directly — the proxy only accepts cookie-authed traffic and blocks `/api/v1/ingest`

## [0.1.8] - 2026-03-08

### Added
- Native syslog TCP hardening: idle timeout, per-connection event limits, CIDR allowlist
- Admin dashboard tab in TUI
- `/api/v1/whoami` endpoint for token identity

### Fixed
- Cap structured data element extraction limits in syslog parser
- Accept bare IPs in syslog CIDR allowlist
- Run syslog WAL writes in `spawn_blocking`
- Handle oversized TCP syslog messages without silent splitting
- Prevent panic on multi-byte UTF-8 in `truncate_query`

### Changed
- Consolidate ingest pipeline into shared `PipelineWriter`
- Extract shared service validation to pipeline module

## [0.1.7] - 2026-03-05

### Added
- TUI redesign: horizontal tab bar, vim-style splash screen, status bar relocation
- Enhanced query error messages with "did you mean?" suggestions and real-time validation
- Native syslog listener for network appliances (UDP + TCP)
- `/api/v1/whoami` endpoint

### Fixed
- Prioritize error status over stale results in render dispatch
- Focus editor when loading query from history or saved tabs
- Show Running indicator and accept execute keys from results pane
- Use actual viewport height for results pane scrolling

## [0.1.6] - 2026-02-28

### Fixed
- Hot buffer visibility race — insert events synchronously during ingest, eliminating the window where freshly ingested events were invisible to queries
- Remove `mark_draining` to eliminate a second hot buffer event invisibility race

### Changed
- Field filter syntax changed from `:` to `=` (e.g. `service=nginx` instead of `service:nginx`)

### Added
- Named profiles and inline token support in CLI config

## [0.1.5] - 2026-02-25

### Fixed
- Add postinst script to create `trawl` system user before service start

## [0.1.4] - 2026-02-24

### Fixed
- Correct APT `Filename:` path prefix doubling in release workflow

## [0.1.3] - 2026-02-23

### Changed
- Sync deb package version from git tag in release workflow
- Rename server deb package from `trawld` to `trawl-server`

## [0.1.2] - 2026-02-21

### Added
- Modular Vector configs with debian drop-in architecture
- `workflow_dispatch` trigger for manual releases

### Fixed
- GPG armor header warning tolerance on secret import
- Set git identity before gh-pages init commit
- Pin apt sources to amd64 in cross-compile, add arm64 from ports

## [0.1.1] - 2026-02-18

### Changed
- Project renamed from "fleet" to "trawl" — all crate names, env vars, paths, configs updated

### Fixed
- Fast-hash feature gate to reduce argon2 cost in tests
- Force libduckdb-sys rebuild to survive cache cleaning
- Relocate auto-generated TLS certs and fix debian packaging
- Free disk space on CI runners before build

## [0.1.0] - 2026-02-15

### Added
- Initial release
- Pipeline-oriented DSL with 14 pipe stages and 17 aggregation functions
- Parquet storage with DuckDB query execution
- WAL → hourly parquet → daily rollup compaction pipeline
- Hot buffer for sub-millisecond event visibility
- SSE streaming with in-memory compiled filters
- argon2id API key authentication with four roles
- TLS with auto-generated self-signed certificates
- Prometheus metrics at `/metrics`
- Internal telemetry (server monitors itself via `service=trawld`)
- Interactive TUI with syntax highlighting and schema browser
- CLI with table, JSON, CSV, and parquet output formats
- Embedded mode for querying local parquet files without a server
- Scheduled queries with cron-style execution
- CI/CD with cross-compiled binaries, .deb packages, and APT repository

[Unreleased]: https://github.com/jakub/trawl/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/jakub/trawl/compare/v0.3.3...v0.4.0
[0.3.3]: https://github.com/jakub/trawl/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/jakub/trawl/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/jakub/trawl/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/jakub/trawl/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/jakub/trawl/compare/v0.1.8...v0.2.0
[0.1.8]: https://github.com/jakub/trawl/compare/v0.1.7...v0.1.8
[0.1.7]: https://github.com/jakub/trawl/compare/v0.1.6...v0.1.7
[0.1.6]: https://github.com/jakub/trawl/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/jakub/trawl/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/jakub/trawl/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/jakub/trawl/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/jakub/trawl/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/jakub/trawl/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/jakub/trawl/releases/tag/v0.1.0
