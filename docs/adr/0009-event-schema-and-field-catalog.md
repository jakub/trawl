# A declared event schema, and a catalog that makes column types authoritative at write time

status: accepted (2026-07-29), amended (2026-07-29) after adversarial review — see Amendment

trawl has no data model. `validate_event` requires exactly one field — `service` —
and every other top-level JSON key becomes a physical parquet column whose type
DuckDB infers per compaction batch, per service. Types are therefore a property
of ingest history rather than of the field, and nothing reconciles them.

Three data-loss paths follow directly from that, and all three are silent:

- **Column drop.** `read_json` runs at `maximum_depth=2`, so one level of nesting
  flattens into dotted names. When that produces a collision, `build_wal_batch`
  (`compaction.rs:1102-1123`) falls back to an explicit ten-column list — the
  stable Vector envelope. `read_json` with an explicit `columns={...}` reads only
  those, so every other field is dropped for the whole service-hour. Logged at
  warn as `compaction_fallback`; the operator is not told which columns vanished.
- **History drop.** Compaction groups by service, so two services that use one
  field name with different types only collide at query time under
  `read_parquet(union_by_name=true)`. `hot_cold_conflicts`
  (`trawl-engine/src/executor.rs:254`) compares the hot snapshot against cold and
  so finds nothing to coerce for a cold-versus-cold conflict. The query falls to
  `HotOnly` and returns HTTP 200 carrying hot-buffer rows only, with the entire
  parquet history omitted.
- **Meaning drift.** `where duration > 1000` means a numeric comparison or a
  lexical one depending on which service compacted first. `"9" > "1000"` is true
  lexically. No error is raised.

The common cause is that column types are decided **per file, at write time,
independently** — and reconciled, if at all, at read time where there is no
authority to appeal to. Every mitigation in the tree today
(`coerce_complex_columns_to_varchar`, `hot_cold_conflicts`,
`emit_with_hot_source_coerced`, the `HotOnly` degradation) is a read-time patch
over a write-time omission.

Separately, the schema that exists by convention is undocumented and inconsistent.
`WELL_KNOWN_LOG_FIELDS` (`trawl-api/src/value.rs:173`) names five privileged
fields but only reorders result columns. `host` is auto-filled from the peer IP,
which is the relay's address behind any collector. There is no field for ingest
time, so a scheduled report over `last=1h` on event time silently misses
late-arriving data. Nothing records what the server changed about an event.

## Decisions

- **Column types are authoritative at write time, held in a field catalog.** Three
  tables in the postgres database trawld already owns: `field_types` (the pins,
  keyed on field name alone), `field_services` (which services carry which field),
  `field_conflicts` (what was cast and how many rows were nulled). Postgres is
  already a hard boot dependency, so this adds no coupling. The invariant is one
  sentence: *every parquet file's column types match the catalog, therefore
  `union_by_name` across any set of files can never conflict.*
- **Compaction is the only writer, and it pins on the first complete batch.** Not
  the first value — a service whose first event carries `duration: "N/A"` must not
  pin VARCHAR forever. Compaction already infers types per batch, so pinning from
  the batch costs nothing and survives a single bad sample. A field absent from
  the catalog is inserted; a field present and matching passes through; a field
  present and differing is `TRY_CAST` to the pinned type with the NULL count
  recorded.
- **The SQL emitter never reads the catalog.** `trawl-core` stays pure with no
  I/O, as documented. Because conformance is established at write time, query time
  has nothing to resolve — `read_parquet(glob, union_by_name=true)` needs no help.
  The hot-buffer snapshot conforms via an in-process pin cache, since snapshot
  generation sits on the query path and must not do postgres I/O. This *removes*
  code: the conflict-detection and coerced-retry path
  (`executor.rs:119-129`, `hot_cold_conflicts`, `emit_with_hot_source_coerced`)
  exists solely to paper over a conflict the catalog now prevents.
- **A type conflict nulls the value; it does not rename it.** A type-suffixed
  sibling column (`duration__varchar`) was the alternative. It fails on
  ergonomics: the two columns live in different files, so a single-service query
  against the minority service returns *only* `duration__varchar` with no
  `duration` present and no signal a rename occurred — the team that lost the
  naming race uses a mangled field name in its own queries permanently. The deeper
  reason is that `'10'` versus `'00:10'` is a unit and meaning conflict wearing a
  type conflict's clothes. Renaming preserves the bytes but not the ability to
  aggregate them jointly, which is the only thing preservation would have been
  for — and `_raw` already preserves the bytes. The correct fix is always the
  sender renaming its own field, done by whoever knows the units.
- **Widening to VARCHAR is rejected.** It looks friendlier and is strictly worse:
  historical files keep the old type, so the union still conflicts until a rollup
  rewrites them, and once converged every numeric predicate becomes a lexical
  comparison. It keeps the current defect and adds silent wrongness on top.
- **Per-service types are rejected.** They are locally correct and break the query
  worth having — "which service is slow" spans services by construction.
- **A repin uses deferred activation.** Changing a pin would reintroduce the
  defect: old files hold the old type, new files the new one. So the field is
  marked dirty, the *old* type continues to be written until the rollup has
  rewritten every affected file, and the pin then flips atomically. There is no
  window in which the invariant is broken. Rollup already rewrites files, so no
  new subsystem is required. A repin is therefore not instantaneous, which is the
  correct cost. **[Superseded by the Amendment: this mechanism is unsound —
  repin moves to a shadow-generation rewrite in its own issue.]**
- **The event schema is declared, and `_` marks handling.** The namespace rule is
  that a leading underscore denotes metadata about how the record was handled, and
  no prefix denotes data about the event. Client-settability is declared per field
  rather than encoded in the name — `severity` is system-derived but carries no
  prefix, because it is content and because `_severity>=warn` would appear in
  every runbook ever written.

  | Field | Type | Presence |
  |---|---|---|
  | `_time` | TIMESTAMP | required — event time |
  | `_ingested` | TIMESTAMP | required — server-stamped, client cannot set |
  | `_raw` | VARCHAR | required — most original form available |
  | `_repairs` | VARCHAR | nullable — codes for what the server changed |
  | `env` | VARCHAR | required — path segment 1, mirrored as column |
  | `service` | VARCHAR | required — path segment 2, mirrored as column |
  | `host` | VARCHAR | required — column, not a path segment |
  | `severity` | INTEGER | derived — OTel SeverityNumber 1-24 |
  | `severity_text` | VARCHAR | optional — original severity text |
  | `message` | VARCHAR | convention — the important part of the line |

- **Two path dimensions, fixed depth:**
  `data/{env}/{date}/{HH}/{service}.parquet`. A dimension earns a path position
  only if it is always present, low and boundedly cardinal, filtered in most
  queries, and has a permanent natural nesting order. Time and `service` already
  qualified; `env` is the only addition that does. Per-env retention stays O(1)
  (`unlink data/prod/{date}`) and global retention becomes O(number of envs),
  which closes the per-class retention gap without breaking the existing
  invariant. `env` and `service` are mirrored as columns so `stats by env` needs
  no string surgery.
- **`host` is a column, not a path segment.** It is high cardinality, so a path
  position yields file-per-host-per-hour: thousands of small parquet files, ruined
  compression, slow glob expansion, and more files in every union — increasing
  exposure to the type-conflict class rather than reducing it. It is also a fact
  about the event, not a filing decision. Splunk treats `host` as both, and that
  duality is part of why its model feels accreted.
- **Hierarchical stream naming is rejected.** A single dotted `stream`
  ("prod.web.nginx") replacing index/service/sourcetype was considered and fails
  on four counts. `sanitize_service_for_filename` (`ingest/wal.rs:24-35`) maps
  every non-alphanumeric character to `_` and passes `_` through, so
  `prod.web.nginx`, `prod.web_nginx`, and `prod_web.nginx` collide in one file —
  prefix pruning over those names cannot be made correct.
  `sanitizes_dotted_service` (`source.rs:534-541`) proves dotted service names
  already exist, falsifying the backward-compatibility claim. A fixed hierarchy
  encodes one query axis permanently, which is why Graphite's dotted paths lost to
  Prometheus label sets. And OTel identity is a bag of resource attributes, so
  bag-to-ordered-path is lossy and arbitrary — it would fight the planned
  ingestor.
- **`index` and `sourcetype` are not adopted.** `index` exists in Splunk for
  multi-tenant ACLs and license metering, both out of scope; its only applicable
  job is retention class, which `env` now provides. `sourcetype` is a parser
  concept, and Vector does the parsing — it also correlates almost perfectly with
  `service`.
- **OpenTelemetry is an ingestion concern, not a storage model.** An OTel ingestor
  maps OTel feeds into this schema. Nested `Resource`/`Attributes` are not
  adopted: flat columns are the performance bet, and nesting is precisely what
  triggers the column-drop defect. OTel's `SeverityNumber` and `ObservedTimestamp`
  are adopted, as `severity` and `_ingested`, because both express something the
  current model cannot.
- **Severity is normalized server-side, and never rejected.** `severity` holds the
  OTel ladder (TRACE 1-4, DEBUG 5-8, INFO 9-12, WARN 13-16, ERROR 17-20, FATAL
  21-24); `severity_text` holds the original verbatim. The DSL exposes `level` as
  an alias for `severity`, so `level=error` compiles to a range predicate and
  `level>=warn` becomes expressible — the most-wanted severity query, and one no
  string field can answer. Severity name *tokens* are lowercased before lookup;
  comparison operators stay case-sensitive everywhere, because a per-column
  exception to `=` is exactly the kind of local irregularity that makes a language
  feel evolved. Syslog numerics 0-7 are accepted and **inverted** on the way in
  (syslog 0 is Emergency, OTel counts upward); a naive passthrough would silently
  invert every severity in the corpus. Precedence is client-supplied `severity` if
  an integer 1-24, else derived from `severity_text`, else NULL — which makes the
  OTel ingestor a straight passthrough. An unmappable value leaves `severity` NULL
  and `severity_text` intact, and increments a counter.
- **`_raw` is a server guarantee, not a client requirement.** It holds the most
  original form available: the collector's pre-parse line when present, otherwise
  the event exactly as it arrived on the wire. The shipped Vector configs
  (`config/vector/local-dev.toml:68, 98`) do not preserve a pre-parse original, so
  requiring clients to supply it would make the mandatory field the one our own
  collectors do not produce. It must be captured *before* `fill_defaults` runs
  (`ingest/handler.rs:207`), or the server's own auto-filled values appear inside
  "what arrived". Cost is roughly 1.3-1.6x at rest after Snappy on a repetitive
  column; wire size is unchanged because the field is derived server-side.
- **`message` is retained alongside `_raw`.** They coincide only when nothing was
  parsed. `message` is the human-readable part, is what every result table shows,
  and is what bare-word search targets today (`emitter/search.rs:138`). Bare
  search additionally covers `_raw` where present.
- **Repair when the server has an honest answer; reject when it would guess.**
  `_time` is repaired from arrival (per ADR-0008). `env` is repaired from a
  configured `default_env`, because most single-site deployments have exactly one.
  `host` is repaired from the peer IP, except when the peer is a configured
  trusted relay — behind a collector the peer address is the collector's and the
  value would be confidently wrong. `service` is **rejected**: it is the shard key
  and the filename, the server cannot invent it honestly, and `service=unknown`
  silently merges unrelated logs into one file, which is worse than a rejection.
- **`_repairs` records what the server accepted after changing.** It is the mirror
  of the existing `RejectReason`/`RejectCounts` mechanism
  (`ingest/handler.rs:141-142`), which records what was refused. NULL when the
  event was untouched; otherwise comma-separated codes from a closed Rust enum —
  the same principle as ADR-0006's "roles are data, permissions are code", without
  which the field becomes a junk drawer. The complete ingest-time set is
  `host.from_peer`, `env.defaulted`, `time.from_ingest`, `time.out_of_range`,
  `severity.unmapped`, `field.truncated`. Accompanied by
  `trawl_ingest_repairs_total{code, service}`, because alerting on a counter is far
  cheaper than querying logs. `service` is the only client-controlled label in the
  tree and the prometheus recorder never evicts a counter series, so the label is
  admitted for the first 256 distinct services seen per process and collapses to
  `service="<other>"` thereafter — otherwise any key holding `ingest` grows the
  registry without bound by posting fresh service names.

  It is not named `_tags`: in observability that word means user-supplied labels
  (Datadog tags, Prometheus labels), and reusing it for server annotations is a
  vocabulary collision. It is not a `VARCHAR[]`, because a LIST is a complex type
  that `is_complex_type` (`compaction.rs:1176`) would coerce to VARCHAR anyway.
  And it does not carry prose: embedding identity (`prod.nginx 10.0.4.55: ...`)
  makes the column high-cardinality and destroys
  `stats count() by _repairs, service`, while the identity is already in the
  `env`, `service`, and `host` columns.
- **Schema-level events do not appear in `_repairs`.** Type conflicts happen at
  compaction, and marking individual rows there would mean rewriting them
  mid-batch. They are recorded in `field_conflicts`, keyed by field and service
  with a row count. That is sufficient, because the action is always "fix the
  sender" and never "fix the row".
- **Enforced ingest-time typing for dynamic fields is rejected.** Zero-config
  ingestion is the product: point Vector at it and it works. Every constraint is a
  place where someone's logs are refused at 03:00 and discovered a week later.
  `props.conf` is the most-complained-about part of Splunk and mapping conflicts
  the most-complained-about part of Elasticsearch. The catalog delivers the
  diagnostic value of a declared schema — visibility, stable types, drift
  detection — without the rejection risk.
- **Legacy data is dropped.** A clean cut, with no migration shims or
  dual-read paths.

## Consequences

- ADR-0008's *policy* is preserved unchanged: the partition key is never
  hard-CAST, malformed time is substituted and preserved, and the event is never
  rejected for it. Only the naming moves — `timestamp` becomes `_time`. Its
  `timestamp_invalid` column becomes redundant once `_raw` is mandatory, since the
  original value is then present in `_raw` and findable via the
  `time.from_ingest` code; it is retired at the schema cutover rather than
  carried forward. Issue #49 lands first on the current names and its value is
  immediate; the cutover subsumes it.
- Sequencing is forced by the dependency graph. The catalog is load-bearing for
  the history-drop fix *and* for the schema cutover, so it is built first and once.
  The column-drop fix rides with it, because both touch the same
  `read_wal_to_table` / `build_wal_batch` seam. The cutover lands last, when the
  catalog beneath it is proven. **[Superseded by the Amendment: the cutover now
  lands first — #49 → #52 → #50 → #51 — because the dependency actually runs
  the other way.]**
- `/api/v1/schema` stops being a corpus-wide `DESCRIBE` behind a TTL cache and
  becomes a `SELECT`. It gains the per-service answer it cannot give today, which
  is the question a schema browser is actually for.
- Embedded mode (`trawl query --data '*.parquet'`) needs no catalog: the files it
  reads are conformant because a server holding a catalog wrote them.
- An early bad sample can still pin a field wrongly. This is why the repin path
  exists, and why pinning is per-batch rather than per-value. A repin requires a
  new compile-time `Permission` variant under ADR-0006.
- `_repairs` costs essentially nothing at rest: it is NULL-dominant so parquet
  run-length encodes it, and dictionary-encoded when present so it is
  bloom-filter eligible under the pinned false-positive ratio. Adding a code later
  is not a schema change — it is a new string value in a column that already
  exists.
- The closed-core alternative — a fixed physical schema with a flat `attrs` JSON
  column and catalog-driven promotion of hot keys — is deliberately left
  available. It makes all three defect classes structurally impossible rather
  than prevented by invariant, and the catalog is its prerequisite. Choosing the
  catalog now keeps that door open without a second migration.

## Amendment (2026-07-29): adversarial review round

An outside review (codex / gpt-5.6-sol) challenged the slice specs. Every claim
was verified — the engine claims by execution against the bundled DuckDB (1.5.5,
crate `duckdb 1.10505.0`) — and all held. Four decisions change; the schema
itself is untouched.

- **Resequenced: #49 → #52 (schema cutover) → #50 (catalog) → #51 (surface).**
  The original order made #50's guarantees circular: conflict nulling claimed
  `_raw` recoverability before `_raw` existed, and the conformance invariant
  was claimed over a legacy corpus only #52 would drop. Cutover-first means one
  destructive reset, `_raw` before any nulling, and an invariant true from
  birth. In this order the cutover touches no postgres state (the catalog does
  not exist yet), so "legacy data is dropped" gains a concrete mechanism: a
  filesystem-only, restartable EPOCH-marker protocol with a tested
  boot-decision table, the old root set aside (never deleted by trawl) for the
  operator. The residual gap — files written between #52 and #50 are unpinned —
  is closed by a boot conformance pass in #50 (seed pins, rewrite nonconforming
  files, gate query serving until done), deliberately built as the embryo of
  the repin rewriter.
- **Repin via deferred activation is withdrawn as unsound.** Rewriting visible
  files one at a time creates the old/new type mixture the invariant forbids,
  for the whole rewrite window, regardless of when the pin flips; and the daily
  rollup structurally cannot do the rewriting — it skips today
  (`compaction.rs:261`) and skips already-consolidated days
  (`compaction.rs:291`), so consolidated daily files would never be rewritten.
  "Rollup already rewrites files, so no new subsystem" was false. The correct
  design is a **shadow-generation rewrite** — build the conformed dataset
  invisibly (hardlink unaffected files, rewrite affected), catch up, switch the
  active root and pin atomically — split to its own issue (#53) pending its own
  design pass. Until it ships a wrong pin has no supported remedy; the pin
  algorithm below makes one rare.
- **"Pin on first complete batch" is now an explicit algorithm**, because the
  engine disproved the delegation: DuckDB 1.5.5 infers `JSON` for a mixed
  string/int column **and for an all-null column**, and `HUGEINT` for
  `u64::MAX` — none representable in the catalog. Canonical types are exactly
  five (`BOOLEAN | BIGINT | DOUBLE | TIMESTAMP | VARCHAR`) with a normalization
  table from DuckDB's inferences; mixed or out-of-range batches pin via a
  deterministic candidate ladder (`TRY_CAST` success ≥90% over non-null values,
  `BIGINT → DOUBLE → TIMESTAMP → BOOLEAN`, else `VARCHAR`); an all-null column
  under an unpinned field defers the pin and is omitted from the file
  (`union_by_name` reads absence as NULL). `merge_with_existing`'s cast
  fallback joins the read-time patches on the deletion list.
- **Path encoding is injective by validation, and `env` is allowlisted.**
  `sanitize_service_for_filename` was lossy (`.` and space collapse to `_`),
  and `env` would have added a second client-controlled path component on top
  of it. The sanitizer is deleted: `env` must match `[a-z0-9_-]{1,32}` and be a
  member of a configured allowlist (`[ingest] envs`, implicitly
  `[default_env]` when omitted — zero-config preserved); a present-but-unlisted
  env **hard-rejects**, because repairing it into `default_env` would misfile
  data in the wrong path root permanently — path placement, unlike a column,
  cannot be re-attributed later — and rejection is loud and proximate where
  repair is quiet and permanent. The allowlist also catches typos, which a
  cardinality cap never would. The service charset drops space and keeps dots;
  filenames carry the name verbatim. The allowlist gates writes only; existing
  directories stay readable.
- **Bare search over `_raw` is whole-event search, and is documented as such.**
  "Bare search additionally covers `_raw`" was written while `_raw` meant a
  collector's pre-parse line. The canonical-pre-repair fallback makes the
  server-filled `_raw` a JSON object, so a bare term matches anywhere in the
  event — another field's value (`nginx` finds `service=nginx`) *and* a field
  name (`debug` finds `debug_mode`), with negation excluding on the same
  basis. The behaviour stands: finding an event without knowing which field
  holds the term is the whole reason bare words exist, and confining the
  search to `message` would make `_raw` coverage a no-op for every event our
  own collectors produce (none of them send a pre-parse line). What was wrong
  was the telling — the DSL reference now states the consequence and points at
  field filters (`message=/debug/`) for a match confined to one column, and
  execution tests pin both directions so the semantics are a decision rather
  than a surprise.
- Smaller corrections, folded into the slices: `_raw` is the client's string
  `_raw` verbatim when supplied, else the **canonical pre-repair
  serialization** — "exactly as it arrived on the wire" was false (the WAL
  re-serializes, and array-batched events have no per-event wire form);
  client-sent `_ingested` / `_repairs` are stripped with a `meta.stripped`
  repair code; the severity contract carries exact token→number and inverted
  syslog→number tables with single-letter aliases, and `level` is declared in
  the derivation chain (consumed at ingest, not stored — the DSL alias would
  shadow it); `field_services` rows are ever-observed and consumers window on
  `last_seen`; `_ingested` makes late data *detectable* — ingest-time report
  windowing is future work, and "makes scheduled reports correct" overclaimed.
  In #49, execution confirmed all four trigger variants say `Conversion Error`,
  so the suspected warning-free degradation path does not exist; the ingest
  timestamp grammar is chrono-parsed and canonicalized to UTC — as shipped it is
  wider than strict RFC 3339 (ISO 8601 basic offsets, offset-less date-times,
  bare dates, space separator, `YYYY/MM/DD`), so ADR-0008 holds the normative
  grammar and the cutover carries it forward rather than narrowing it; fallback
  provenance is per-row via `read_json(filename=true)` and travels as a reserved
  `_trawl_wal_file` column that ingest strips from client events (retired at the
  cutover alongside `timestamp_invalid`), and an unexpected query
  failure with cold files present returns an error rather than hot-only
  success.
